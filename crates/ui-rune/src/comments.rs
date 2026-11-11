//! Diff comments: notes pinned to a line of the changes pane, staged on the
//! composer and folded into the next prompt as plain text.
//!
//! Ported from the gpui UI's `comments.rs`; the badge extractor follows with the
//! transcript module.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CommentSide {
    Old,
    New,
}

impl CommentSide {
    pub fn tag(self) -> &'static str {
        match self {
            Self::Old => "L",
            Self::New => "R",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffComment {
    pub id: String,
    /// The diff's own path — post-rename when the file moved.
    pub path: String,
    /// Pre-rename path, when the file moved. `None` otherwise.
    pub old_path: Option<String>,
    pub side: CommentSide,
    pub line: u32,
    pub body: String,
}

impl DiffComment {
    pub fn new(
        path: impl Into<String>,
        side: CommentSide,
        line: u32,
        body: impl Into<String>,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            path: path.into(),
            old_path: None,
            side,
            line,
            body: body.into(),
        }
    }

    /// Tag the comment with the file's pre-rename path.
    pub fn renamed_from(mut self, old_path: Option<impl Into<String>>) -> Self {
        self.old_path = old_path.map(Into::into);
        self
    }

    pub fn anchor(&self) -> (CommentSide, u32) {
        (self.side, self.line)
    }

    /// The path the line number is valid in.
    pub fn cite_path(&self) -> &str {
        match self.side {
            CommentSide::Old => self.old_path.as_deref().unwrap_or(&self.path),
            CommentSide::New => &self.path,
        }
    }

    pub fn location(&self) -> String {
        format!("{}:{}", self.cite_path(), self.line)
    }
}

pub const COMMENT_ONLY_TEXT: &str = "Address the review comments below.";

pub const COMMENT_BLOCK_HEADER: &str = "Comments on the diff (each cites the file and line it belongs to; L = line number in the original file, R = in the changed file):";

fn side_marker(side: CommentSide) -> String {
    format!(" ({}): ", side.tag())
}

pub fn with_comments(text: &str, comments: &[DiffComment]) -> String {
    if comments.is_empty() {
        return text.to_string();
    }
    let bullets: Vec<String> = comments
        .iter()
        .map(|comment| {
            let body = comment.body.trim().replace('\n', "\n  ");
            format!(
                "- {}{}{body}",
                comment.location(),
                side_marker(comment.side)
            )
        })
        .collect();
    let body = if text.is_empty() {
        COMMENT_ONLY_TEXT
    } else {
        text
    };
    format!("{body}\n\n{COMMENT_BLOCK_HEADER}\n{}", bullets.join("\n"))
}

pub fn chip_label(count: usize) -> String {
    if count == 1 {
        "1 comment".to_string()
    } else {
        format!("{count} comments")
    }
}

pub const CARD_PAD_V: f32 = 20.0;
pub const CARD_HEADER_HEIGHT: f32 = 22.0;
pub const CARD_LINE_HEIGHT: f32 = 18.0;
pub const DRAFT_CARD_HEIGHT: f32 = 116.0;
const CARD_GAP: f32 = 6.0;
const CARD_WRAP_COLUMNS: usize = 64;
const CARD_MAX_LINES: usize = 8;

/// Wraps are guessed, not measured: the changes pane sizes bodies by arithmetic
/// to drive the fold tween.
pub fn card_body_lines(body: &str) -> usize {
    body.lines()
        .map(|line| line.chars().count().div_ceil(CARD_WRAP_COLUMNS).max(1))
        .sum::<usize>()
        .clamp(1, CARD_MAX_LINES)
}

pub fn card_height(body: &str) -> f32 {
    CARD_PAD_V + CARD_HEADER_HEIGHT + card_body_lines(body) as f32 * CARD_LINE_HEIGHT + CARD_GAP
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comment(path: &str, side: CommentSide, line: u32, body: &str) -> DiffComment {
        DiffComment::new(path, side, line, body)
    }

    #[test]
    fn empty_set_leaves_the_prompt_untouched() {
        assert_eq!(with_comments("ship it", &[]), "ship it");
    }

    #[test]
    fn comments_append_as_located_bullets() {
        let staged = vec![
            comment("src/main.rs", CommentSide::New, 42, "early-return here"),
            comment("src/lib.rs", CommentSide::Old, 7, "why was this dropped?"),
        ];
        let out = with_comments("look at these", &staged);
        assert!(out.starts_with("look at these\n\n"));
        assert!(out.contains("- src/main.rs:42 (R): early-return here"));
        assert!(out.contains("- src/lib.rs:7 (L): why was this dropped?"));
    }

    #[test]
    fn a_renamed_file_cites_the_side_the_line_lives_in() {
        let old = comment("new_name.rs", CommentSide::Old, 7, "why dropped?")
            .renamed_from(Some("old_name.rs"));
        let new =
            comment("new_name.rs", CommentSide::New, 12, "nit").renamed_from(Some("old_name.rs"));
        assert_eq!(old.path, "new_name.rs");
        assert_eq!(new.path, "new_name.rs");
        let out = with_comments("x", &[old, new]);
        assert!(out.contains("- old_name.rs:7 (L): why dropped?"));
        assert!(out.contains("- new_name.rs:12 (R): nit"));
    }

    #[test]
    fn card_height_grows_with_body_lines() {
        assert!(card_height("two\nlines") > card_height("one"));
        assert_eq!(card_height(""), card_height("one"));
    }
}
