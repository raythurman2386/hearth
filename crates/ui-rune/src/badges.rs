//! Message badges: pills standing in for structured context a user message
//! carries.
//!
//! A feature stages context on the composer, folds it into the prompt as plain
//! text, and registers an [`Extractor`]. [`split`] lifts that block back out of
//! the sent message so the transcript draws a pill instead of the bullets the
//! agent reads. Nothing here knows what a diff comment is.

use rune::{Color, El, TextStyle, el, row, text};

use crate::theme::Theme;

#[derive(Debug, Clone)]
pub struct MessageBadge {
    /// The glyph the pill leads with.
    pub icon: &'static rune::Icon,
    pub label: String,
    /// Empty means the label says everything and the pill carries no card.
    pub details: Vec<BadgeDetail>,
}

/// One row of a hover card. Three generic slots, so a new badge kind fills them
/// without this module learning what it cites.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadgeDetail {
    /// `src/main.rs:42`, a URL, a commit subject.
    pub location: String,
    /// `L`/`R` for a diff side, a status, a count.
    pub tag: Option<String>,
    pub body: String,
}

/// Returns the text with this feature's block removed, plus the pill replacing
/// it. `None` when the message carries nothing of that kind.
pub type Extractor = fn(&str) -> Option<(String, MessageBadge)>;

/// Adapt the comments feature's neutral badge contract to a transcript pill.
fn comment_badge(text: &str) -> Option<(String, MessageBadge)> {
    let (rest, badge) = crate::comments::extract_badge(text)?;
    let details = badge
        .details
        .into_iter()
        .map(|detail| BadgeDetail {
            location: detail.location,
            tag: detail.tag,
            body: detail.body,
        })
        .collect();
    Some((
        rest,
        MessageBadge {
            icon: &crate::icons::CHAT_ROUND_LINE,
            label: badge.label,
            details,
        },
    ))
}

const EXTRACTORS: &[Extractor] = &[comment_badge];

/// Each extractor sees what the previous ones left behind, so two features can
/// ride the same prompt.
pub fn split(text: &str) -> (String, Vec<MessageBadge>) {
    let mut rest = text.to_string();
    let mut badges = Vec::new();
    for extract in EXTRACTORS {
        if let Some((stripped, badge)) = extract(&rest) {
            rest = stripped;
            badges.push(badge);
        }
    }
    (rest, badges)
}

/// The composer sizes its strip arithmetically, so this cannot be a
/// measurement.
pub const BADGE_HEIGHT: f32 = 24.0;

const PILL_RADIUS: f32 = 8.0;
const ICON_SIZE: f32 = 12.0;
const TEXT_SIZE: f32 = 12.0;

/// One badge pill. `k` scopes the hover card and must be stable per pill.
pub fn render(k: impl Into<String>, badge: &MessageBadge, theme: &Theme) -> El {
    let _ = k.into();
    let mut pill = row()
        .h(BADGE_HEIGHT)
        .gap(6.0)
        .px(8.0)
        .center_y()
        .radius(PILL_RADIUS)
        .fill(theme.ink(0.06))
        .child(rune::icon(badge.icon, ICON_SIZE).color(theme.text_muted.alpha(0.7)))
        .child(
            text(badge.label.clone())
                .font(TextStyle::new(TEXT_SIZE, 500.0))
                .color(theme.text_muted)
                .single_line(),
        );
    if !badge.details.is_empty() {
        pill = pill.help(details_text(badge));
    }
    pill
}

/// The hover card's text form (the Rune port uses the system tooltip rather
/// than a bespoke popover surface).
pub fn details_text(badge: &MessageBadge) -> String {
    badge
        .details
        .iter()
        .map(|detail| match &detail.tag {
            Some(tag) => format!("{} [{}]\n{}", detail.location, tag, detail.body),
            None => format!("{}\n{}", detail.location, detail.body),
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The card body for callers that mount their own popover surface.
pub fn card(badge: &MessageBadge, theme: &Theme) -> El {
    let mut out = rune::col().w(320.0).padding(6.0).gap(4.0);
    for detail in &badge.details {
        let mut meta = row().gap(6.0).center_y();
        meta = meta.child(
            text(detail.location.clone())
                .font(TextStyle::new(10.0, 400.0).mono())
                .color(theme.text_faint)
                .truncate()
                .min_w(0.0),
        );
        if let Some(tag) = &detail.tag {
            meta = meta.child(
                text(tag.clone())
                    .font(TextStyle::new(10.0, 500.0))
                    .color(theme.text_faint)
                    .px(4.0)
                    .radius(3.0)
                    .fill(theme.ink(0.10)),
            );
        }
        out = out.child(
            row()
                .gap(8.0)
                .padding(8.0)
                .radius(6.0)
                .fill(theme.ink(0.05))
                .child(el().w(2.0).fill(theme.solid.alpha(0.35)))
                .child(
                    rune::col().grow(1.0).min_w(0.0).gap(4.0).child(meta).child(
                        text(detail.body.clone())
                            .font(TextStyle::new(TEXT_SIZE, 400.0).lh(16.0))
                            .color(theme.text)
                            .wrap(),
                    ),
                ),
        );
    }
    out
}

/// The pill's icon tint (kept for parity with the gpui helper).
pub fn icon_tint(theme: &Theme) -> Color {
    theme.text_muted.alpha(0.7)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comments::{CommentSide, DiffComment, with_comments};

    #[test]
    fn a_plain_message_carries_no_badges() {
        let (text, badges) = split("just a prompt");
        assert_eq!(text, "just a prompt");
        assert!(badges.is_empty());
    }

    #[test]
    fn a_sent_comment_block_becomes_one_pill() {
        let staged = vec![
            DiffComment::new("a.rs", CommentSide::New, 3, "fix"),
            DiffComment::new("b.rs", CommentSide::Old, 9, "why"),
        ];
        let (text, badges) = split(&with_comments("do", &staged));
        assert_eq!(text, "do");
        assert_eq!(badges.len(), 1);
        assert_eq!(badges[0].label, "2 comments");
    }

    #[test]
    fn the_card_carries_one_row_per_comment() {
        let staged = vec![
            DiffComment::new("src/main.rs", CommentSide::New, 42, "early-return here"),
            DiffComment::new("src/lib.rs", CommentSide::Old, 7, "why was this dropped?"),
        ];
        let (_, badges) = split(&with_comments("look", &staged));
        let details = &badges[0].details;
        assert_eq!(details.len(), 2);
        assert_eq!(details[0].location, "src/main.rs:42");
        assert_eq!(details[0].tag.as_deref(), Some("R"));
        assert_eq!(details[0].body, "early-return here");
        assert_eq!(details[1].location, "src/lib.rs:7");
        assert_eq!(details[1].tag.as_deref(), Some("L"));
    }

    #[test]
    fn a_multiline_body_rejoins_its_continuation_lines() {
        let staged = vec![DiffComment::new(
            "a.rs",
            CommentSide::New,
            3,
            "first\nsecond",
        )];
        let (_, badges) = split(&with_comments("x", &staged));
        assert_eq!(badges[0].details[0].body, "first\nsecond");
    }

    #[test]
    fn a_path_with_a_colon_still_splits_on_the_side_marker() {
        let staged = vec![DiffComment::new("odd:name.rs", CommentSide::New, 5, "hm")];
        let (_, badges) = split(&with_comments("x", &staged));
        assert_eq!(badges[0].details[0].location, "odd:name.rs:5");
        assert_eq!(badges[0].details[0].body, "hm");
    }

    #[test]
    fn a_comment_only_send_keeps_its_stand_in_body() {
        let staged = vec![DiffComment::new("a.rs", CommentSide::New, 3, "fix")];
        let (text, badges) = split(&with_comments("", &staged));
        assert_eq!(text, crate::comments::COMMENT_ONLY_TEXT);
        assert_eq!(badges[0].label, "1 comment");
    }

    #[test]
    fn detail_text_names_the_side() {
        let staged = vec![DiffComment::new("a.rs", CommentSide::New, 3, "fix")];
        let (_, badges) = split(&with_comments("x", &staged));
        assert!(details_text(&badges[0]).contains("[R]"));
    }
}
