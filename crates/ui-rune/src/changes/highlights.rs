//! Syntax highlighting for diff lines: mapping hunks to old/new source
//! documents, excerpts and full-file highlights.
//!
//! Colour stays outside the documents: the pure layer stores source line spans
//! and the Rune layer paints them from `theme.syntax`, so an appearance change
//! recolours already-parsed documents.

use std::sync::Arc;

use hearth_syntax::{HighlightKind, LanguageId as Lang};
use rune::Span;

use crate::theme::Theme;

use super::{DiffLine, FileDiff, FileStatus, LineKind, SourceLineRef, SourceSide};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DiffHighlights {
    pub old: Option<Arc<hearth_syntax::HighlightedDocument>>,
    pub new: Option<Arc<hearth_syntax::HighlightedDocument>>,
}

impl DiffHighlights {
    pub fn source_ref(&self, line: &DiffLine) -> Option<SourceLineRef> {
        match line.kind {
            LineKind::Del => line.old_no.map(|line_number| SourceLineRef {
                side: SourceSide::Old,
                line_number,
            }),
            LineKind::Add => line.new_no.map(|line_number| SourceLineRef {
                side: SourceSide::New,
                line_number,
            }),
            LineKind::Context => line
                .new_no
                .filter(|_| self.new.is_some())
                .map(|line_number| SourceLineRef {
                    side: SourceSide::New,
                    line_number,
                })
                .or_else(|| {
                    line.old_no.map(|line_number| SourceLineRef {
                        side: SourceSide::Old,
                        line_number,
                    })
                }),
            LineKind::Meta => None,
        }
    }

    pub fn spans(&self, line: &DiffLine) -> &[hearth_syntax::HighlightSpan] {
        let Some(source_ref) = self.source_ref(line) else {
            return &[];
        };
        let document = match source_ref.side {
            SourceSide::Old => self.old.as_deref(),
            SourceSide::New => self.new.as_deref(),
        };
        document
            .and_then(|document| document.lines.get(source_ref.line_number as usize - 1))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

/// Paint-only Rune spans for one diff line, coloured from `theme.syntax`.
/// Layout never changes: only run colours are attached.
pub fn spans_for_line(
    line: &DiffLine,
    highlights: Option<&DiffHighlights>,
    theme: &Theme,
) -> Vec<Span> {
    let Some(highlights) = highlights else {
        return Vec::new();
    };
    highlights
        .spans(line)
        .iter()
        .map(|span| {
            Span::new(span.range.start, span.range.end).color(theme.syntax.color(span.kind))
        })
        .collect()
}

/// Every distinct token kind this document paints — used by tests and by the
/// theme-contrast smoke check.
pub fn painted_kinds(highlights: &DiffHighlights) -> Vec<HighlightKind> {
    let mut kinds: Vec<HighlightKind> = [&highlights.old, &highlights.new]
        .into_iter()
        .flatten()
        .flat_map(|document| document.lines.iter().flat_map(|line| line.iter()))
        .map(|span| span.kind)
        .collect();
    kinds.sort_by_key(|kind| kind.precedence());
    kinds.dedup();
    kinds
}

const MAX_EXCERPT_SOURCE_LINES: usize = 200_000;

fn excerpt_side(
    file: &FileDiff,
    side: SourceSide,
    language: Lang,
    path: &str,
) -> Option<Arc<hearth_syntax::HighlightedDocument>> {
    let max_line = file
        .hunks
        .iter()
        .flat_map(|hunk| &hunk.lines)
        .filter_map(|line| match side {
            SourceSide::Old => line.old_no,
            SourceSide::New => line.new_no,
        })
        .max()
        .unwrap_or(0) as usize;
    if max_line > MAX_EXCERPT_SOURCE_LINES {
        return None;
    }
    let mut lines = vec![Vec::new(); max_line];
    for hunk in &file.hunks {
        let visible = hunk
            .lines
            .iter()
            .filter_map(|line| {
                let number = match side {
                    SourceSide::Old => line.old_no,
                    SourceSide::New => line.new_no,
                }?;
                (line.kind != LineKind::Meta).then_some((number, line.text.as_str()))
            })
            .collect::<Vec<_>>();
        if visible.is_empty() {
            continue;
        }
        let source = visible
            .iter()
            .map(|(_, text)| *text)
            .collect::<Vec<_>>()
            .join("\n");
        let document = hearth_syntax::highlight(hearth_syntax::HighlightRequest {
            source: &source,
            path: Some(path),
            fence_tag: None,
        })
        .ok()?;
        for ((number, _), spans) in visible.into_iter().zip(document.lines) {
            lines[number as usize - 1] = spans;
        }
    }
    Some(Arc::new(hearth_syntax::HighlightedDocument {
        language,
        lines,
    }))
}

pub fn excerpt_highlights(file: &FileDiff, language: Lang) -> Option<DiffHighlights> {
    if !hearth_syntax::supports_language(language) {
        return None;
    }
    let old = if file.status == FileStatus::Added {
        None
    } else {
        Some(excerpt_side(
            file,
            SourceSide::Old,
            language,
            file.old_path.as_deref().unwrap_or(&file.path),
        )?)
    };
    let new = if file.status == FileStatus::Deleted {
        None
    } else {
        Some(excerpt_side(file, SourceSide::New, language, &file.path)?)
    };
    Some(DiffHighlights { old, new })
}

fn sources_match_patch(file: &FileDiff, response: &hearth_proto::CheckoutFileDiffText) -> bool {
    let old = response
        .old_text
        .as_deref()
        .map(|source| source.lines().collect::<Vec<_>>());
    let new = response
        .new_text
        .as_deref()
        .map(|source| source.lines().collect::<Vec<_>>());
    file.hunks.iter().flat_map(|hunk| &hunk.lines).all(|line| {
        let actual = match line.kind {
            LineKind::Del => line
                .old_no
                .and_then(|number| old.as_ref()?.get(number as usize - 1).copied()),
            LineKind::Add => line
                .new_no
                .and_then(|number| new.as_ref()?.get(number as usize - 1).copied()),
            LineKind::Context => line
                .new_no
                .and_then(|number| new.as_ref()?.get(number as usize - 1).copied())
                .or_else(|| {
                    line.old_no
                        .and_then(|number| old.as_ref()?.get(number as usize - 1).copied())
                }),
            LineKind::Meta => return true,
        };
        actual == Some(line.text.as_str())
    })
}

pub fn full_highlights(
    file: &FileDiff,
    language: Lang,
    response: &hearth_proto::CheckoutFileDiffText,
) -> Option<DiffHighlights> {
    if response.stale
        || response.binary
        || response.truncated
        || !sources_match_patch(file, response)
    {
        return None;
    }
    let parse = |source: &str, path: &str| {
        hearth_syntax::highlight(hearth_syntax::HighlightRequest {
            source,
            path: Some(path),
            fence_tag: None,
        })
        .ok()
        .map(Arc::new)
    };
    let old = match response.old_text.as_deref() {
        Some(source) => Some(parse(
            source,
            file.old_path.as_deref().unwrap_or(&file.path),
        )?),
        None => None,
    };
    let new = match response.new_text.as_deref() {
        Some(source) => Some(parse(source, &file.path)?),
        None => None,
    };
    if old.is_none() && new.is_none() && hearth_syntax::supports_language(language) {
        return None;
    }
    Some(DiffHighlights { old, new })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::{parse_patch, DiffLine, FileDiff, FileStatus, Hunk, LineKind};
    use crate::theme::Theme;

    fn line(kind: LineKind, old_no: Option<u32>, new_no: Option<u32>, text: &str) -> DiffLine {
        DiffLine {
            kind,
            old_no,
            new_no,
            text: text.into(),
        }
    }

    #[test]
    fn full_diff_highlights_map_old_new_and_context_by_source_line() {
        let old_source = "fn old() {\n    let value = 1;\n}\n";
        let new_source = "fn new() {\n    let value = 2;\n}\n";
        let parse = |source| {
            Arc::new(
                hearth_syntax::highlight(hearth_syntax::HighlightRequest {
                    source,
                    path: Some("src/lib.rs"),
                    fence_tag: None,
                })
                .unwrap(),
            )
        };
        let highlights = DiffHighlights {
            old: Some(parse(old_source)),
            new: Some(parse(new_source)),
        };
        let deleted = line(LineKind::Del, Some(1), None, "fn old() {");
        let added = line(LineKind::Add, None, Some(1), "fn new() {");
        let context = line(LineKind::Context, Some(2), Some(2), "    let value = 2;");
        assert_eq!(
            highlights.source_ref(&deleted),
            Some(SourceLineRef {
                side: SourceSide::Old,
                line_number: 1
            })
        );
        assert_eq!(
            highlights.source_ref(&added),
            Some(SourceLineRef {
                side: SourceSide::New,
                line_number: 1
            })
        );
        assert_eq!(
            highlights.source_ref(&context),
            Some(SourceLineRef {
                side: SourceSide::New,
                line_number: 2
            })
        );
        assert!(
            highlights
                .spans(&deleted)
                .iter()
                .any(|span| span.kind == HighlightKind::Function)
        );
        assert!(
            highlights
                .spans(&added)
                .iter()
                .any(|span| span.kind == HighlightKind::Function)
        );
    }

    #[test]
    fn excerpt_parses_old_and_new_hunks_as_separate_documents() {
        let file = FileDiff {
            path: "src/lib.rs".into(),
            old_path: None,
            status: FileStatus::Modified,
            binary: false,
            notices: vec![],
            hunks: vec![Hunk {
                header: "@@ -1,3 +1,3 @@".into(),
                lines: vec![
                    line(LineKind::Context, Some(1), Some(1), "/* start"),
                    line(LineKind::Del, Some(2), None, "old body"),
                    line(LineKind::Add, None, Some(2), "new body"),
                    line(LineKind::Context, Some(3), Some(3), "end */"),
                ],
            }],
            additions: 1,
            deletions: 1,
            max_line: 3,
        };
        let highlights = excerpt_highlights(&file, Lang::Rust).expect("excerpt");
        let deleted = &file.hunks[0].lines[1];
        let added = &file.hunks[0].lines[2];
        assert!(
            highlights
                .spans(deleted)
                .iter()
                .any(|span| span.kind == HighlightKind::Comment)
        );
        assert!(
            highlights
                .spans(added)
                .iter()
                .any(|span| span.kind == HighlightKind::Comment)
        );
    }

    #[test]
    fn mismatched_full_sources_are_rejected_atomically() {
        let file = FileDiff {
            path: "src/lib.rs".into(),
            old_path: None,
            status: FileStatus::Modified,
            binary: false,
            notices: vec![],
            hunks: vec![Hunk {
                header: "@@ -1 +1 @@".into(),
                lines: vec![
                    line(LineKind::Del, Some(1), None, "let old = 1;"),
                    line(LineKind::Add, None, Some(1), "let new = 2;"),
                ],
            }],
            additions: 1,
            deletions: 1,
            max_line: 1,
        };
        let response = hearth_proto::CheckoutFileDiffText {
            diff_checksum: "sum".into(),
            old_text: Some("let old = 1;\n".into()),
            new_text: Some("different snapshot\n".into()),
            old_content_hash: None,
            new_content_hash: None,
            binary: false,
            truncated: false,
            stale: false,
        };
        assert!(!sources_match_patch(&file, &response));
        assert!(full_highlights(&file, Lang::Rust, &response).is_none());
    }

    #[test]
    fn rune_spans_are_paint_only_and_take_theme_colours() {
        let files = parse_patch(crate::changes::PATCH);
        let highlights = excerpt_highlights(&files[0], Lang::Rust);
        let theme = Theme::dark();
        let Some(highlights) = highlights else {
            return; // language support can be compiled out
        };
        let line = &files[0].hunks[0].lines[0];
        let spans = spans_for_line(line, Some(&highlights), &theme);
        let source = highlights.spans(line);
        assert_eq!(spans.len(), source.len());
        for (span, raw) in spans.iter().zip(source) {
            assert_eq!(
                (span.start as usize, span.end as usize),
                (raw.range.start, raw.range.end)
            );
            assert_eq!(span.color, Some(theme.syntax.color(raw.kind)));
        }
        assert!(!painted_kinds(&highlights).is_empty());
    }
}
