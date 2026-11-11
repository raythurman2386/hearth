//! Highlight store: one bounded LRU of tokenized code blocks.
//!
//! The gpui UI tokenized on a background task and applied paint-only run
//! colours when the document landed. The Rune port tokenizes synchronously
//! through [`hearth_syntax`] on first use — tree-sitter highlighting is linear
//! and sub-millisecond for transcript-sized fences — then keeps the neutral
//! document in [`HighlightStore`] (LRU, 512 slots) and in the shared
//! [`SyntaxHighlightCache`]. Colours still live outside the cache, so an
//! appearance change recolours without parsing again.

use std::collections::HashMap;
use std::sync::Arc;

use hearth_syntax::{HighlightedDocument, LanguageId};

use crate::markdown::parser::{Block, BlockTree};
use crate::syntax_cache::{DocumentHighlightKey, SyntaxHighlightCache};

use super::model::ToolDetail;

/// Cap on cached code-block highlight slots.
pub const MAX_HIGHLIGHT_ENTRIES: usize = 512;

struct HighlightEntry {
    key: DocumentHighlightKey,
    document: Arc<HighlightedDocument>,
    touched: u64,
}

/// Cache of tokenized code blocks keyed by `(row id, block ix)`.
#[derive(Default)]
pub struct HighlightStore {
    entries: HashMap<(String, usize), HighlightEntry>,
    cache: SyntaxHighlightCache,
    touch_counter: u64,
}

impl HighlightStore {
    fn touch(&mut self, slot_key: &(String, usize)) {
        self.touch_counter = self.touch_counter.wrapping_add(1);
        if let Some(entry) = self.entries.get_mut(slot_key) {
            entry.touched = self.touch_counter;
        }
    }

    fn insert_entry(
        &mut self,
        slot_key: (String, usize),
        key: DocumentHighlightKey,
        document: Arc<HighlightedDocument>,
    ) {
        self.touch_counter = self.touch_counter.wrapping_add(1);
        self.entries.insert(
            slot_key.clone(),
            HighlightEntry {
                key,
                document,
                touched: self.touch_counter,
            },
        );
        while self.entries.len() > MAX_HIGHLIGHT_ENTRIES {
            let victim = self
                .entries
                .iter()
                .filter(|(key, _)| **key != slot_key)
                .min_by_key(|(_, entry)| entry.touched)
                .map(|(key, _)| key.clone());
            let Some(victim) = victim else {
                break;
            };
            self.entries.remove(&victim);
        }
    }

    /// Tokens for one code block, parsing on first use.
    pub fn get(
        &mut self,
        row_id: &str,
        block_ix: usize,
        language: LanguageId,
        code: &str,
    ) -> Option<Arc<HighlightedDocument>> {
        let slot_key = (row_id.to_string(), block_ix);
        let document_key = DocumentHighlightKey::new(language, code);
        if let Some(entry) = self.entries.get(&slot_key)
            && entry.key == document_key
        {
            let document = entry.document.clone();
            self.touch(&slot_key);
            return Some(document);
        }
        let document = if let Some(document) = self.cache.get(&document_key) {
            document
        } else {
            let document = Arc::new(
                hearth_syntax::highlight(hearth_syntax::HighlightRequest {
                    source: code,
                    path: None,
                    fence_tag: Some(fence_tag(language)),
                })
                .ok()?,
            );
            self.cache.insert(document_key, document.clone());
            document
        };
        self.insert_entry(slot_key, document_key, document.clone());
        Some(document)
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Test seam for the bounded-insertion policy.
    #[cfg(test)]
    fn insert_for_test(&mut self, slot_key: (String, usize), language: LanguageId, code: &str) {
        let key = DocumentHighlightKey::new(language, code);
        let document = Arc::new(
            hearth_syntax::highlight(hearth_syntax::HighlightRequest {
                source: code,
                path: None,
                fence_tag: Some(fence_tag(language)),
            })
            .unwrap(),
        );
        self.insert_entry(slot_key, key, document);
    }
}

/// The canonical fence tag for a language (hearth_syntax accepts aliases).
pub fn fence_tag(language: LanguageId) -> &'static str {
    match language {
        LanguageId::Rust => "rust",
        LanguageId::JavaScript => "javascript",
        LanguageId::Jsx => "jsx",
        LanguageId::TypeScript => "typescript",
        LanguageId::Tsx => "tsx",
        LanguageId::Python => "python",
        LanguageId::Go => "go",
        LanguageId::Json => "json",
        LanguageId::Jsonc => "jsonc",
        LanguageId::Bash => "bash",
        LanguageId::Toml => "toml",
        LanguageId::Markdown => "markdown",
        LanguageId::Html => "html",
        LanguageId::Css => "css",
        LanguageId::Yaml => "yaml",
        LanguageId::C => "c",
        LanguageId::Cpp => "cpp",
        LanguageId::CSharp => "csharp",
        LanguageId::Java => "java",
        LanguageId::Kotlin => "kotlin",
        LanguageId::Swift => "swift",
        LanguageId::Ruby => "ruby",
        LanguageId::Php => "php",
        LanguageId::Sql => "sql",
        LanguageId::Lua => "lua",
        LanguageId::Dockerfile => "dockerfile",
        LanguageId::Nix => "nix",
        LanguageId::Make => "make",
    }
}

/// Request highlights for the code blocks of a tree. `only` limits to one block
/// index (split rows); `None` covers the whole tree.
#[allow(dead_code)]
pub fn code_highlight_for(
    store: &mut HighlightStore,
    row_id: &str,
    tree: &BlockTree,
    only: Option<usize>,
) -> HashMap<usize, Option<Arc<HighlightedDocument>>> {
    let mut out = HashMap::new();
    for (ix, top) in tree.blocks.iter().enumerate() {
        if only.is_some_and(|only| only != ix) {
            continue;
        }
        if let Block::CodeBlock { language, code } = &top.block
            && let Some(lang) = language
                .as_deref()
                .and_then(hearth_syntax::language_for_alias)
        {
            out.insert(ix, store.get(row_id, ix, lang, code));
        }
    }
    out
}

/// Request the two source sides of an inline tool diff.
pub fn tool_diff_highlight_for(
    store: &mut HighlightStore,
    row_id: &str,
    tool_ix: usize,
    detail: &ToolDetail,
) -> Option<Arc<crate::changes::DiffHighlights>> {
    let ToolDetail::Diff {
        file,
        old_text,
        new_text,
    } = detail
    else {
        return None;
    };
    let cache_row = format!("{row_id}#tool-diff-{tool_ix}");
    let old = match old_text {
        Some(source) => {
            let path = file.old_path.as_deref().unwrap_or(&file.path);
            let lang = hearth_syntax::language_for_path(path)?;
            Some(store.get(&cache_row, 0, lang, source)?)
        }
        None => None,
    };
    let new = match new_text {
        Some(source) => {
            let lang = hearth_syntax::language_for_path(&file.path)?;
            Some(store.get(&cache_row, 1, lang, source)?)
        }
        None => None,
    };
    Some(Arc::new(crate::changes::DiffHighlights { old, new }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::parser::parse_full;

    #[test]
    fn store_reuses_a_document_until_the_content_changes() {
        let mut store = HighlightStore::default();
        let a = store
            .get("r0", 0, LanguageId::Rust, "fn main() {}")
            .unwrap();
        let b = store
            .get("r0", 0, LanguageId::Rust, "fn main() {}")
            .unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        let c = store
            .get("r0", 0, LanguageId::Rust, "fn other() {}")
            .unwrap();
        assert!(!Arc::ptr_eq(&a, &c));
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn store_is_bounded_at_the_cap() {
        let mut store = HighlightStore::default();
        for ix in 0..MAX_HIGHLIGHT_ENTRIES + 17 {
            store.insert_for_test((format!("row{ix}"), 0), LanguageId::Rust, "fn main() {}");
        }
        assert_eq!(store.len(), MAX_HIGHLIGHT_ENTRIES);
    }

    #[test]
    fn code_blocks_only_reach_the_store() {
        let tree = parse_full("text\n\n```rust\nlet x = 1;\n```\n");
        let mut store = HighlightStore::default();
        let highlights = code_highlight_for(&mut store, "row", &tree, Some(1));
        assert!(highlights.contains_key(&1));
        assert!(highlights[&1].is_some());
    }

    #[test]
    fn empty_store_starts_empty() {
        assert!(HighlightStore::default().is_empty());
    }
}
