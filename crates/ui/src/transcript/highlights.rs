use super::Transcript;
use super::model::ToolDetail;
use crate::markdown::parser::{Block, BlockTree};
use crate::syntax_cache::{DocumentHighlightKey, SyntaxHighlightCache};
use gpui::{Context, SharedString, Task};
use hearth_syntax::LanguageId as Lang;
use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::Instant;

// ---------------------------------------------------------------------------
// Highlight store (background, time-sliced, paint-only)
// ---------------------------------------------------------------------------

pub(super) struct HighlightEntry {
    key: DocumentHighlightKey,
    document: Option<Weak<hearth_syntax::HighlightedDocument>>,
    _task: Option<Task<()>>,
    /// Monotonic "last requested" stamp (see [`HighlightStore::touch`]) — the
    /// LRU key for the store's cap.
    touched: u64,
}

/// Cap on cached code-block highlight slots: one entry per rendered code
/// block `(row id, block ix)`, plus one slot per diff side. A chat switch
/// clears the store wholesale, so this is only a backstop for ONE very long
/// chat — without it, scrolling a transcript with thousands of code blocks
/// would retain a slot (weak document handle + task) for every block ever
/// seen. The least recently requested slots are evicted first.
pub(super) const MAX_HIGHLIGHT_ENTRIES: usize = 512;

/// Cache of tokenized code blocks keyed by `(row id, block ix)`. Tokenization
/// runs on the background executor, time-sliced; results apply as paint-only
/// run colors when they land.
#[derive(Default)]
pub(super) struct HighlightStore {
    pub(super) entries: HashMap<(SharedString, usize), HighlightEntry>,
    cache: SyntaxHighlightCache,
    /// Monotonic recency counter shared by every slot; only the relative
    /// order inside the cap-sized map matters (wrapping is harmless).
    touch_counter: u64,
}

/// Outcome of looking a slot up before tokenizing.
enum SlotProbe {
    /// A tokenize task is already running for this slot.
    InFlight,
    /// Tokens are ready and the cached document is still alive.
    Ready(Arc<hearth_syntax::HighlightedDocument>),
    /// No usable slot — resolve from the document cache or spawn a tokenize.
    Miss,
}

impl HighlightStore {
    /// Look up an existing slot, bumping its recency when it is still usable
    /// (a live document or an in-flight tokenize). A stale slot (different
    /// content, or a dead weak handle) reads as [`SlotProbe::Miss`].
    fn probe(
        &mut self,
        slot_key: &(SharedString, usize),
        document_key: &DocumentHighlightKey,
    ) -> SlotProbe {
        let probe = match self.entries.get(slot_key) {
            Some(entry) if entry.key == *document_key => match entry.document.as_ref() {
                None => SlotProbe::InFlight,
                Some(document) => match document.upgrade() {
                    Some(document) => SlotProbe::Ready(document),
                    None => SlotProbe::Miss,
                },
            },
            _ => SlotProbe::Miss,
        };
        if !matches!(probe, SlotProbe::Miss) {
            self.touch(slot_key);
        }
        probe
    }

    /// Stamp a slot as the most recently requested one. Called on cache hits
    /// too, so a block the user keeps scrolling past cannot be aged out by
    /// unrelated blocks appearing later in the same chat.
    fn touch(&mut self, slot_key: &(SharedString, usize)) {
        self.touch_counter = self.touch_counter.wrapping_add(1);
        if let Some(entry) = self.entries.get_mut(slot_key) {
            entry.touched = self.touch_counter;
        }
    }

    /// Insert a slot (stamped newest) and evict the least recently requested
    /// slots until the store is back within [`MAX_HIGHLIGHT_ENTRIES`].
    /// `slot_key` — the entry just inserted — is never the victim.
    fn insert_entry(&mut self, slot_key: (SharedString, usize), mut entry: HighlightEntry) {
        self.touch_counter = self.touch_counter.wrapping_add(1);
        entry.touched = self.touch_counter;
        self.entries.insert(slot_key.clone(), entry);
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

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Test seam for the LRU policy: exercises the same stamp `request` uses
    /// on a cache hit, without needing a gpui `Context`.
    #[cfg(test)]
    pub(super) fn touch_for_test(&mut self, slot_key: &(SharedString, usize)) {
        self.touch(slot_key);
    }

    /// Test seam for the bounded-insertion policy (`request` needs a gpui
    /// `Context` to spawn tokenization; eviction does not).
    #[cfg(test)]
    pub(super) fn insert_for_test(
        &mut self,
        slot_key: (SharedString, usize),
        key: DocumentHighlightKey,
    ) {
        self.insert_entry(
            slot_key,
            HighlightEntry {
                key,
                document: None,
                _task: None,
                touched: 0,
            },
        );
    }

    /// Current tokens if ready; kicks a background tokenize when stale/missing.
    fn request(
        &mut self,
        row_id: SharedString,
        block_ix: usize,
        lang: Lang,
        code: &str,
        cx: &mut Context<Transcript>,
    ) -> Option<Arc<hearth_syntax::HighlightedDocument>> {
        let slot_key = (row_id.clone(), block_ix);
        let document_key = DocumentHighlightKey::new(lang, code);
        match self.probe(&slot_key, &document_key) {
            SlotProbe::InFlight => return None,
            SlotProbe::Ready(document) => return Some(document),
            SlotProbe::Miss => {}
        }
        if let Some(document) = self.cache.get(&document_key) {
            self.insert_entry(
                slot_key,
                HighlightEntry {
                    key: document_key,
                    document: Some(Arc::downgrade(&document)),
                    _task: None,
                    touched: 0,
                },
            );
            return Some(document);
        }
        let code = code.to_string();
        let source_bytes = code.len();
        let task = cx.spawn(async move |this, cx| {
            let started = Instant::now();
            let document = cx
                .background_executor()
                .spawn(async move {
                    hearth_syntax::highlight(hearth_syntax::HighlightRequest {
                        source: &code,
                        path: None,
                        fence_tag: Some(match lang {
                            Lang::Rust => "rust",
                            Lang::JavaScript => "javascript",
                            Lang::Jsx => "jsx",
                            Lang::TypeScript => "typescript",
                            Lang::Tsx => "tsx",
                            Lang::Python => "python",
                            Lang::Go => "go",
                            Lang::Json => "json",
                            Lang::Jsonc => "jsonc",
                            Lang::Bash => "bash",
                            Lang::Toml => "toml",
                            Lang::Markdown => "markdown",
                            Lang::Html => "html",
                            Lang::Css => "css",
                            Lang::Yaml => "yaml",
                            Lang::C => "c",
                            Lang::Cpp => "cpp",
                            Lang::CSharp => "csharp",
                            Lang::Java => "java",
                            Lang::Kotlin => "kotlin",
                            Lang::Swift => "swift",
                            Lang::Ruby => "ruby",
                            Lang::Php => "php",
                            Lang::Sql => "sql",
                            Lang::Lua => "lua",
                            Lang::Dockerfile => "dockerfile",
                            Lang::Nix => "nix",
                            Lang::Make => "make",
                        }),
                    })
                    .ok()
                })
                .await;
            this.update(cx, |transcript, cx| {
                if let Some(document) = document {
                    let document = Arc::new(document);
                    let retained = transcript
                        .highlights
                        .cache
                        .insert(document_key, document.clone());
                    if let Some(entry) = transcript.highlights.entries.get_mut(&slot_key)
                        && entry.key == document_key
                    {
                        tracing::debug!(
                            language = ?lang,
                            source_bytes,
                            spans = document.lines.iter().map(Vec::len).sum::<usize>(),
                            elapsed_us = started.elapsed().as_micros() as u64,
                            "syntax highlight ready"
                        );
                        entry.document = retained.then(|| Arc::downgrade(&document));
                        cx.notify();
                    }
                }
            })
            .ok();
        });
        self.insert_entry(
            (row_id, block_ix),
            HighlightEntry {
                key: document_key,
                document: None,
                _task: Some(task),
                touched: 0,
            },
        );
        None
    }
}
impl Transcript {
    /// Request highlights for the code blocks of a tree. `only` limits to one
    /// block index (split rows); `None` covers the whole tree (live rows).
    pub(super) fn code_highlight_for(
        &mut self,
        row_id: &SharedString,
        tree: &Arc<BlockTree>,
        only: Option<usize>,
        cx: &mut Context<Self>,
    ) -> HashMap<usize, Option<Arc<hearth_syntax::HighlightedDocument>>> {
        let mut out = HashMap::new();
        for (ix, top) in tree.blocks.iter().enumerate() {
            if only.is_some_and(|o| o != ix) {
                continue;
            }
            if let Block::CodeBlock { language, code } = &top.block
                && let Some(lang) = language
                    .as_deref()
                    .and_then(hearth_syntax::language_for_alias)
            {
                out.insert(
                    ix,
                    self.highlights.request(row_id.clone(), ix, lang, code, cx),
                );
            }
        }
        out
    }

    pub(super) fn tool_diff_highlight_for(
        &mut self,
        row_id: &SharedString,
        tool_ix: usize,
        detail: &ToolDetail,
        cx: &mut Context<Self>,
    ) -> Option<Arc<crate::changes::DiffHighlights>> {
        let ToolDetail::Diff {
            file,
            old_text,
            new_text,
        } = detail
        else {
            return None;
        };
        let cache_row: SharedString = format!("{row_id}#tool-diff-{tool_ix}").into();
        let old = match old_text {
            Some(source) => {
                let path = file.old_path.as_deref().unwrap_or(&file.path);
                let lang = hearth_syntax::language_for_path(path)?;
                Some(
                    self.highlights
                        .request(cache_row.clone(), 0, lang, source, cx)?,
                )
            }
            None => None,
        };
        let new = match new_text {
            Some(source) => {
                let lang = hearth_syntax::language_for_path(&file.path)?;
                Some(self.highlights.request(cache_row, 1, lang, source, cx)?)
            }
            None => None,
        };
        Some(Arc::new(crate::changes::DiffHighlights { old, new }))
    }
}
