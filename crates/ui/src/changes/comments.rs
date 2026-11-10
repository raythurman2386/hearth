//! Inline diff comments: the draft/comment entity state and the cards, adders and
//! draft composer rendered over a row.

use super::*;

pub(super) fn comment_state_key(
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

pub(super) fn hash64(parts: &[&str]) -> u64 {
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
pub(super) struct HoverRow {
    path: String,
    side: CommentSide,
    line: u32,
}

pub(super) struct CommentDraft {
    /// Composer the note will stage onto, captured when the card opened. A
    /// draft belongs to the checkout it was written over, so it must not
    /// follow the user onto whatever chat is selected by commit time.
    key: String,
    pub(super) path: String,
    /// The file's pre-rename path, when it moved — carried onto the comment so
    /// an `Old`-side citation names the file that line lives in.
    old_path: Option<String>,
    side: CommentSide,
    pub(super) line: u32,
    pub(super) input: Entity<ComposerInput>,
    _events: Subscription,
}
impl Changes {
    /// Cloned because rendering borrows `self` mutably a moment later.
    pub(super) fn staged_comments(&self, cx: &App) -> Vec<DiffComment> {
        let state = self.state.read(cx);
        state.diff_comments(&state.composer_key()).to_vec()
    }

    pub(super) fn comments_for(&self, path: &str, cx: &App) -> Vec<DiffComment> {
        self.staged_comments(cx)
            .into_iter()
            .filter(|comment| comment.path == path)
            .collect()
    }

    /// The parsed diff's pre-rename path for `path`, when the file moved.
    fn old_path_of(&self, path: &str) -> Option<String> {
        self.parsed
            .as_ref()?
            .files
            .iter()
            .find(|file| file.path == path)?
            .old_path
            .clone()
    }

    /// A draft belongs to the checkout it was opened over. Chat navigation
    /// swaps both the diff under it and the composer it would stage onto, so
    /// the half-written note is dropped rather than following the user across.
    pub(super) fn discard_stale_draft(&mut self, cx: &mut Context<Self>) {
        let key = self.state.read(cx).composer_key();
        if self.draft.as_ref().is_some_and(|draft| draft.key != key) {
            self.draft = None;
            self.sync_comment_rows(cx);
            cx.notify();
        }
    }

    pub(super) fn draft_anchor(&self) -> Option<(String, CommentSide, u32)> {
        self.draft
            .as_ref()
            .map(|draft| (draft.path.clone(), draft.side, draft.line))
    }

    pub(super) fn draft_anchor_in(&self, path: &str) -> Option<(CommentSide, u32)> {
        self.draft
            .as_ref()
            .filter(|draft| draft.path == path)
            .map(|draft| (draft.side, draft.line))
    }

    pub(super) fn sync_comment_rows(&mut self, cx: &mut Context<Self>) {
        if self.parsed.is_none() {
            return;
        }
        let staged = self.staged_comments(cx);
        let draft = self.draft_anchor();
        let key = comment_state_key(&staged, draft.as_ref());
        if key == self.comment_key {
            return;
        }
        self.comment_key = key;
        let Some(parsed) = &self.parsed else {
            return;
        };
        let files = parsed.files.clone();
        for file_ix in (0..self.row_ranges.len().min(files.len())).rev() {
            let file = &files[file_ix];
            // A mid-tween stand-in is the settle sweep's to replace.
            if self
                .folds
                .get(&file.path)
                .is_some_and(|fold| fold.collapsed)
            {
                continue;
            }
            let range = &self.row_ranges[file_ix];
            if self.rows.get(range.start + 1)
                == Some(&DiffRow::FoldingBody {
                    file: file_ix as u32,
                })
            {
                continue;
            }
            let comments: Vec<DiffComment> = staged
                .iter()
                .filter(|comment| comment.path == file.path)
                .cloned()
                .collect();
            let body = body_rows(
                file_ix as u32,
                file,
                &comments,
                self.draft_anchor_in(&file.path),
                self.mode,
            );
            self.replace_file_body(file_ix, body);
        }
        cx.notify();
    }

    pub(super) fn set_hover(
        &mut self,
        path: &str,
        anchor: Option<(CommentSide, u32)>,
        cx: &mut Context<Self>,
    ) {
        let next = anchor.map(|(side, line)| HoverRow {
            path: path.to_string(),
            side,
            line,
        });
        if next != self.hover {
            self.hover = next;
            cx.notify();
        }
    }

    pub(super) fn hovering(&self, path: &str, anchor: (CommentSide, u32)) -> bool {
        self.hover
            .as_ref()
            .is_some_and(|hover| hover.path == path && (hover.side, hover.line) == anchor)
    }

    pub(super) fn clear_hover_at(
        &mut self,
        path: &str,
        anchor: (CommentSide, u32),
        cx: &mut Context<Self>,
    ) {
        if self.hovering(path, anchor) {
            self.hover = None;
            cx.notify();
        }
    }

    fn open_draft(
        &mut self,
        path: String,
        side: CommentSide,
        line: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = cx.new(|cx| ComposerInput::new("Request a change…", cx));
        let events = cx.subscribe(&input, |this: &mut Self, _, event, cx| match event {
            ComposerInputEvent::Submitted => this.commit_draft(cx),
            ComposerInputEvent::Edited => cx.notify(),
            _ => {}
        });
        let handle = input.read(cx).focus_handle(cx);
        let key = self.state.read(cx).composer_key();
        let old_path = self.old_path_of(&path);
        self.draft = Some(CommentDraft {
            key,
            path,
            old_path,
            side,
            line,
            input,
            _events: events,
        });
        window.focus(&handle, cx);
        self.sync_comment_rows(cx);
        cx.notify();
    }

    fn cancel_draft(&mut self, cx: &mut Context<Self>) {
        self.draft = None;
        self.sync_comment_rows(cx);
        cx.notify();
    }

    fn commit_draft(&mut self, cx: &mut Context<Self>) {
        let Some(draft) = self.draft.take() else {
            return;
        };
        let body = draft.input.read(cx).text().trim().to_string();
        if body.is_empty() {
            self.sync_comment_rows(cx);
            cx.notify();
            return;
        }
        let comment =
            DiffComment::new(draft.path, draft.side, draft.line, body).renamed_from(draft.old_path);
        // `draft.key`, not the live one: the note stages onto the composer it
        // was written against even if the selection moved under it.
        let key = draft.key;
        self.state.update(cx, |state, cx| {
            state.add_diff_comment(&key, comment);
            cx.notify();
        });
        self.sync_comment_rows(cx);
        cx.notify();
    }

    fn remove_comment(&mut self, id: &str, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            let key = state.composer_key();
            state.remove_diff_comment(&key, id);
            cx.notify();
        });
        self.sync_comment_rows(cx);
        cx.notify();
    }
}

/// A unified row carries both gutters side by side, and a deletion numbers in
/// the first.
pub fn comment_adder_left(side: CommentSide, gutter_px: f32) -> f32 {
    let column = match side {
        CommentSide::Old => 0.0,
        CommentSide::New => gutter_px,
    };
    ACCENT_BAR_WIDTH + column + (gutter_px - COMMENT_ADDER_SIZE) / 2.0
}

pub(super) fn positioned_adder(left: f32, adder: AnyElement) -> gpui::Div {
    div()
        .absolute()
        .left(px(left))
        .top(px(0.0))
        .h_full()
        .flex()
        .items_center()
        .child(adder)
}

pub(super) fn render_comment_adder(
    path: &str,
    side: CommentSide,
    line: u32,
    theme: &Theme,
    cx: &Context<Changes>,
) -> AnyElement {
    let target = path.to_string();
    div()
        .id(SharedString::from(format!(
            "cmt-add-{path}-{}-{line}",
            side.tag()
        )))
        .size(px(COMMENT_ADDER_SIZE))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(4.0))
        .bg(theme.solid)
        .cursor_pointer()
        .on_click(cx.listener(move |this, _, window, cx| {
            this.open_draft(target.clone(), side, line, window, cx);
        }))
        .child(
            crate::icons::icon(crate::icons::PLUS)
                .size(px(11.0))
                .text_color(theme.on_solid),
        )
        .into_any_element()
}

pub(super) fn render_comment_card(
    comment: &DiffComment,
    theme: &Theme,
    cx: &Context<Changes>,
) -> AnyElement {
    let group: SharedString = format!("cmt-card-{}", comment.id).into();
    let id = comment.id.clone();
    div()
        .group(group.clone())
        .h(px(crate::comments::card_height(&comment.body)))
        .w_full()
        .flex_none()
        .flex()
        .flex_row()
        .bg(crate::theme::ink(0.05))
        // A bar, not a border: it must match ACCENT_BAR_WIDTH exactly or the
        // card's edge steps in and out of the column.
        .child(comment_accent_bar(theme.solid.opacity(0.35)))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .px(px(Theme::SPACE_LG))
                .py(px(crate::comments::CARD_PAD_V / 2.0))
                .child(
                    div()
                        .h(px(crate::comments::CARD_HEADER_HEIGHT))
                        .flex_none()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .child(
                            crate::icons::icon(crate::icons::CHAT_ROUND_LINE)
                                .size(px(12.0))
                                .text_color(theme.text_faint),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .font_family(theme.font_mono.clone())
                                .text_size(px(11.0))
                                .text_color(theme.text_faint)
                                .child(SharedString::from(comment.location())),
                        )
                        .child(
                            div()
                                .id(SharedString::from(format!("cmt-remove-{}", comment.id)))
                                .flex_none()
                                .size(px(16.0))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(4.0))
                                .cursor_pointer()
                                .opacity(0.0)
                                .group_hover(group, |s| s.opacity(1.0))
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.remove_comment(&id, cx)),
                                )
                                .child(
                                    crate::icons::icon(crate::icons::CLOSE_CIRCLE)
                                        .size(px(12.0))
                                        .text_color(theme.text_muted),
                                ),
                        ),
                )
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        // Height is analytic, so an over-long body clips
                        // inside the card rather than past the fold height.
                        .overflow_hidden()
                        .text_size(px(12.0))
                        .line_height(px(crate::comments::CARD_LINE_HEIGHT))
                        .text_color(theme.text_dim)
                        .child(SharedString::from(comment.body.clone())),
                ),
        )
        .into_any_element()
}

fn comment_accent_bar(color: gpui::Hsla) -> gpui::Div {
    div().w(px(ACCENT_BAR_WIDTH)).h_full().flex_none().bg(color)
}

/// Mirrors [`DiffComment::cite_path`] for the not-yet-staged note.
pub(super) fn draft_cite_path(draft: &CommentDraft) -> &str {
    match draft.side {
        CommentSide::Old => draft.old_path.as_deref().unwrap_or(&draft.path),
        CommentSide::New => &draft.path,
    }
}

/// Fixed height, so an open draft never fights the fold tween.
pub(super) fn render_comment_draft(
    path: &str,
    line: u32,
    input: Entity<ComposerInput>,
    theme: &Theme,
    cx: &Context<Changes>,
) -> AnyElement {
    div()
        .h(px(crate::comments::DRAFT_CARD_HEIGHT))
        .w_full()
        .flex_none()
        .flex()
        .flex_row()
        .bg(crate::theme::ink(0.08))
        .child(comment_accent_bar(theme.solid.opacity(0.7)))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .px(px(Theme::SPACE_LG))
                .py(px(10.0))
                .child(
                    div()
                        .h(px(crate::comments::CARD_HEADER_HEIGHT))
                        .flex_none()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .child(
                            crate::icons::icon(crate::icons::CHAT_ROUND_LINE)
                                .size(px(12.0))
                                .text_color(theme.text_faint),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .font_family(theme.font_mono.clone())
                                .text_size(px(11.0))
                                .text_color(theme.text_faint)
                                .child(SharedString::from(format!("{path}:{line}"))),
                        ),
                )
                .child(
                    div()
                        .h(px(46.0))
                        .flex_none()
                        .overflow_hidden()
                        .text_size(px(12.0))
                        .child(input.into_any_element()),
                )
                .child(
                    div()
                        .h(px(28.0))
                        .flex_none()
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_end()
                        .gap(px(6.0))
                        .child(
                            comment_action("cmt-cancel", "Cancel", false, theme)
                                .on_click(cx.listener(|this, _, _, cx| this.cancel_draft(cx))),
                        )
                        .child(
                            comment_action("cmt-commit", "Comment", true, theme)
                                .on_click(cx.listener(|this, _, _, cx| this.commit_draft(cx))),
                        ),
                ),
        )
        .into_any_element()
}

fn comment_action(
    id: &'static str,
    label: &'static str,
    primary: bool,
    theme: &Theme,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(px(22.0))
        .px(px(10.0))
        .flex()
        .items_center()
        .rounded(px(6.0))
        .text_size(px(11.0))
        .font_weight(gpui::FontWeight::MEDIUM)
        .cursor_pointer()
        .when(primary, |el| el.bg(theme.solid).text_color(theme.on_solid))
        .when(!primary, |el| {
            el.text_color(motion::hover_blend(id, theme.text_muted, theme.text))
                .bg(motion::hover_blend(
                    id,
                    gpui::transparent_black(),
                    theme.element_hover,
                ))
                .on_hover(motion::hover_listener(id))
        })
        .child(SharedString::from(label))
}
