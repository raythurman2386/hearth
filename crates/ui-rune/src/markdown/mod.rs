//! Hearth's markdown stack on Rune — pulldown-cmark parse into a
//! [`parser::BlockTree`], block-level incremental reparse for streaming, and a
//! render step that rebuilds each block's source and draws it with
//! [`rune_kit::Markdown`] (GFM tables, task lists, fenced code with syntax
//! paints, diffs, mermaid and one selection across blocks).
//!
//! Design (docs/research/mugen-pretext.md §2, docs/rune-port.md):
//! - the parse is block-granular and append-incremental: streaming reparses only
//!   from the last stable top-level block boundary;
//! - hanging inline markers (`**bold`, `[link](url…`) are auto-closed in the
//!   streaming *display* parse only ([`mend`]), so closing markers never
//!   reflow already-painted text; the canonical parse settles honestly on
//!   completion;
//! - code colouring is paint-only: rune-kit's [`rune_kit::syntax::Highlighter`]
//!   is wired to `hearth_syntax`, so token colours are the same ones the
//!   changes pane paints and layout never depends on them;
//! - the streaming fade is rune-kit's word reveal (`.reveal`), which replaces
//!   the gpui paint veil; [`veil`] keeps the original cadence math and tests.

pub mod mend;
pub mod parser;
pub mod render;
pub mod veil;

pub use parser::{Block, BlockTree, IncrementalParser, InlineRun, InlineStyle, parse_full};
