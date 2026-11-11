//! Inline diff comments: the draft state and the cards, adders and draft
//! composer rendered over a row.
//!
//! `crate::comments` owns the pure [`DiffComment`] model; this module owns the
//! pane-local state and painting.

use std::hash::{Hash, Hasher};

use rune::{el, row, text, text_input, El, TextStyle, TextEdit};

use crate::comments::{CommentSide, DiffComment};
use crate::state::App;
use crate::theme::Theme;

use super::ACCENT_BAR_WIDTH;

pub fn comment_state_key(
    comments: &[DiffComment],
    draft: Option<&(String, CommentSide, u32)>,
) -> u64 {
    let mut parts: Vec<String> = comments.iter().map(|comment| comment.id.clone()).collect();
    if let Some((path, side, line)) = draft {
        parts.push(format!("draft:{path}:{}:{line}", side.tag()));
    }
    let refs: Vec<&str> = parts.iter().map(String::as_str).collect();
    hash64(&refs)
}

pub fn hash64(parts: &[&str]) -> u64 {
    let mut hasher = std::hash::DefaultHasher::new();
    for p in parts {
        p.hash(&mut hasher);
    }
    hasher.finish()
}

/// The line the pointer is on. Only one element per anchor ever takes the
/// hover — the unified row, or a split row's right column — so the anchor
/// alone identifies it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoverRow {
    pub path: String,
    pub side: CommentSide,
    pub line: u32,
}

/// The one open draft.
pub struct CommentDraft {
    /// Composer the note will stage onto, captured when the card opened. A
    /// draft belongs to the checkout it was written over, so it must not
    /// follow the user onto whatever chat is selected by commit time.
    pub key: String,
    pub path: String,
    /// The file's pre-rename path, when it moved — carried onto the comment so
    /// an `Old`-side citation names the file that line lives in.
    pub old_path: Option<String>,
    pub side: CommentSide,
    pub line: u32,
    pub edit: TextEdit,
}

impl std::fmt::Debug for CommentDraft {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommentDraft")
            .field("key", &self.key)
            .field("path", &self.path)
            .field("side", &self.side)
            .field("line", &self.line)
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// Handlers (take the root state, exactly as Rune handlers do)
// ---------------------------------------------------------------------------

pub(super) fn open_draft(app: &mut App, path: String, side: CommentSide, line: u32) {
    let key = app.changes.comments_key().to_string();
    let old_path = app.changes.old_path_of(&path);
    app.changes.draft = Some(CommentDraft {
        key,
        path,
        old_path,
        side,
        line,
        edit: TextEdit::new(""),
    });
    app.changes.comments_changed();
}

pub(super) fn cancel_draft(app: &mut App) {
    if app.changes.draft.take().is_some() {
        app.changes.comments_changed();
    }
}

pub(super) fn commit_draft(app: &mut App) {
    let Some(draft) = app.changes.draft.take() else {
        return;
    };
    let body = draft.edit.text.trim().to_string();
    if body.is_empty() {
        app.changes.comments_changed();
        return;
    }
    let comment = DiffComment::new(draft.path, draft.side, draft.line, body)
        .renamed_from(draft.old_path);
    // `draft.key`, not the live one: the note stages onto the composer it was
    // written against even if the selection moved under it.
    app.add_diff_comment(&draft.key, comment);
    app.changes.comments_changed();
}

pub(super) fn remove_comment(app: &mut App, id: &str) {
    let key = app.changes.comments_key().to_string();
    app.remove_diff_comment(&key, id);
    app.changes.comments_changed();
}

// ---------------------------------------------------------------------------
// Painting
// ---------------------------------------------------------------------------

/// A unified row carries both gutters side by side, and a deletion numbers in
/// the first.
pub fn comment_adder_left(side: CommentSide, gutter_px: f32) -> f32 {
    let column = match side {
        CommentSide::Old => 0.0,
        CommentSide::New => gutter_px,
    };
    ACCENT_BAR_WIDTH + column + (gutter_px - COMMENT_ADDER_SIZE) / 2.0
}

/// A split row's `+` only ever appears in the right column, which carries one
/// gutter — so the offset is the same for every line. It is measured from the
/// column, not the row: the halves are fluid, so the right one has no
/// absolute left edge to measure from.
pub fn split_adder_left(gutter_px: f32) -> f32 {
    ACCENT_BAR_WIDTH + (gutter_px - COMMENT_ADDER_SIZE) / 2.0
}

pub const COMMENT_ADDER_SIZE: f32 = 16.0;

pub(super) fn render_comment_adder(
    path: &str,
    side: CommentSide,
    line: u32,
    theme: &Theme,
) -> El {
    let target = path.to_string();
    el()
        .key(format!(
            "app/changes/cmt-add/{path}/{}/{line}",
            side.tag()
        ))
        .size(COMMENT_ADDER_SIZE, COMMENT_ADDER_SIZE)
        .center()
        .radius(4.0)
        .fill(theme.solid)
        .cursor(rune::Cursor::Pointer)
        .role("button")
        .label("Add comment")
        .on_press_tx(move |app: &mut App, tx: &mut rune::Tx| {
            open_draft(app, target.clone(), side, line);
            tx.focus("app/changes/draft-input", true);
        })
        .child(
            text("+")
                .font(TextStyle::new(12.0, 600.0))
                .color(theme.on_solid),
        )
}

pub(super) fn render_comment_card(comment: &DiffComment, theme: &Theme) -> El {
    let height = crate::comments::card_height(&comment.body);
    let id = comment.id.clone();
    let body = comment.body.clone();
    let location = comment.location();
    row()
        .key(format!("app/changes/cmt-card/{}", comment.id))
        .w_full()
        .h(height)
        .shrink()
        .fill(theme.ink(0.05))
        .hover_group()
        // A bar, not a border: it must match ACCENT_BAR_WIDTH exactly or the
        // card's edge steps in and out of the column.
        .child(comment_accent_bar(theme.solid.alpha(0.35)))
        .child(
            rune::col()
                .grow(1.0)
                .min_w(0.0)
                .px(Theme::SPACE_LG)
                .py(crate::comments::CARD_PAD_V / 2.0)
                .child(
                    row()
                        .h(crate::comments::CARD_HEADER_HEIGHT)
                        .shrink()
                        .center_y()
                        .gap(6.0)
                        .child(
                            text("•")
                                .font(TextStyle::new(11.0, 400.0))
                                .color(theme.text_faint),
                        )
                        .child(
                            text(location)
                                .font(TextStyle::new(11.0, 400.0))
                                .color(theme.text_faint)
                                .truncate()
                                .grow(1.0),
                        )
                        .child(
                            el()
                                .key(format!("app/changes/cmt-remove/{}", comment.id))
                                .size(16.0, 16.0)
                                .center()
                                .radius(4.0)
                                .cursor(rune::Cursor::Pointer)
                                .role("button")
                                .label("Remove comment")
                                .group_opacity(0.0, 1.0)
                                .on_press(move |app: &mut App| {
                                    remove_comment(app, &id);
                                })
                                .child(
                                    text("×")
                                        .font(TextStyle::new(13.0, 400.0))
                                        .color(theme.text_muted),
                                ),
                        ),
                )
                .child(
                    text(body)
                        .font(TextStyle::new(12.0, 400.0).lh(crate::comments::CARD_LINE_HEIGHT))
                        .color(theme.text_dim)
                        .max_lines(8)
                        .grow(1.0),
                ),
        )
}

/// Mirrors [`DiffComment::cite_path`] for the not-yet-staged note.
pub fn draft_cite_path(draft: &CommentDraft) -> &str {
    match draft.side {
        CommentSide::Old => draft.old_path.as_deref().unwrap_or(&draft.path),
        CommentSide::New => &draft.path,
    }
}

/// Fixed height, so an open draft never fights the fold tween.
pub(super) fn render_comment_draft(draft: &CommentDraft, theme: &Theme) -> El {
    let location = format!("{}:{}", draft_cite_path(draft), draft.line);
    row()
        .key("app/changes/cmt-draft")
        .w_full()
        .h(crate::comments::DRAFT_CARD_HEIGHT)
        .shrink()
        .fill(theme.ink(0.08))
        .child(comment_accent_bar(theme.solid.alpha(0.7)))
        .child(
            rune::col()
                .grow(1.0)
                .min_w(0.0)
                .px(Theme::SPACE_LG)
                .py(10.0)
                .child(
                    row()
                        .h(crate::comments::CARD_HEADER_HEIGHT)
                        .shrink()
                        .center_y()
                        .gap(6.0)
                        .child(
                            text("•")
                                .font(TextStyle::new(11.0, 400.0))
                                .color(theme.text_faint),
                        )
                        .child(
                            text(location)
                                .font(TextStyle::new(11.0, 400.0))
                                .color(theme.text_faint)
                                .truncate()
                                .grow(1.0),
                        ),
                )
                .child(
                    text_input(
                        &draft.edit,
                        TextStyle::new(12.0, 400.0).lh(18.0),
                    )
                    .key("app/changes/draft-input")
                    .w_full()
                    .h(46.0)
                    .shrink()
                    .placeholder("Request a change…", theme.text_faint)
                    .caret(theme.caret, theme.selection)
                    .on_edit(|app: &mut App| app.changes.draft_edit())
                    .on_submit(|app: &mut App, _tx: &mut rune::Tx| commit_draft(app)),
                )
                .child(
                    row()
                        .h(28.0)
                        .shrink()
                        .center_y()
                        .justify_end()
                        .gap(6.0)
                        .child(
                            comment_action("Cancel", false, theme)
                                .key("app/changes/cmt-cancel")
                                .on_press(|app: &mut App| cancel_draft(app)),
                        )
                        .child(
                            comment_action("Comment", true, theme)
                                .key("app/changes/cmt-commit")
                                .on_press(|app: &mut App| commit_draft(app)),
                        ),
                ),
        )
}

fn comment_action(label: &'static str, primary: bool, theme: &Theme) -> El {
    let mut button = el()
        .h(22.0)
        .px(10.0)
        .center()
        .radius(6.0)
        .cursor(rune::Cursor::Pointer)
        .role("button")
        .label(label);
    button = if primary {
        button.fill(theme.solid).child(
            text(label)
                .font(TextStyle::new(11.0, 500.0))
                .color(theme.on_solid),
        )
    } else {
        button
            .fill(rune::Color::TRANSPARENT)
            .hover(theme.element_hover)
            .child(
                text(label)
                    .font(TextStyle::new(11.0, 500.0))
                    .color(theme.text_muted),
            )
    };
    button
}

fn comment_accent_bar(color: rune::Color) -> El {
    el().w(ACCENT_BAR_WIDTH).h_full().shrink().fill(color)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comment(id: &str, path: &str, side: CommentSide, line: u32) -> DiffComment {
        let mut comment = DiffComment::new(path, side, line, "body");
        comment.id = id.to_string();
        comment
    }

    #[test]
    fn comment_key_tracks_staged_ids_and_the_draft() {
        let a = comment("a", "src/main.rs", CommentSide::New, 1);
        let b = comment("b", "src/main.rs", CommentSide::New, 2);
        let empty = comment_state_key(&[], None);
        let one = comment_state_key(std::slice::from_ref(&a), None);
        assert_ne!(empty, one);
        // Same set in the same order is stable.
        assert_eq!(one, comment_state_key(std::slice::from_ref(&a), None));
        // A new id changes the key even at the same path/line.
        let mut a2 = a.clone();
        a2.id = "a2".into();
        assert_ne!(one, comment_state_key(std::slice::from_ref(&a2), None));
        // Order matters (cards index the staged slice).
        assert_ne!(
            comment_state_key(&[a.clone(), b.clone()], None),
            comment_state_key(&[b, a.clone()], None)
        );
        // An open draft is part of the key.
        let draft = ("src/main.rs".to_string(), CommentSide::New, 1);
        assert_ne!(one, comment_state_key(std::slice::from_ref(&a), Some(&draft)));
    }

    #[test]
    fn adders_sit_inside_their_gutter_column() {
        // Unified: the old side's adder is in the first gutter, the new side's
        // in the second, both centred in a gutter after the accent bar.
        let gutter = 36.0;
        let old = comment_adder_left(CommentSide::Old, gutter);
        let new = comment_adder_left(CommentSide::New, gutter);
        assert_eq!(old, ACCENT_BAR_WIDTH + (gutter - COMMENT_ADDER_SIZE) / 2.0);
        assert_eq!(new, old + gutter);
        assert_eq!(split_adder_left(gutter), old);
    }
}
