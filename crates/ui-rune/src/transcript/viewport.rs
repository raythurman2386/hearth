//! Transcript viewport policy.
//!
//! Rune owns the scroll physics (`ScrollSpec { stick_bottom: true }`): it
//! follows content growth, releases the end on user input, and re-pins when the
//! view lands back inside the end band. That replaces the gpui per-frame
//! `StickSpring`; this module keeps the pure policy constants, the own-turn
//! reservation math, and the session-local viewport snapshots (LRU 256) that
//! the row model and tests share.

use std::collections::{HashMap, VecDeque};

use crate::theme::Theme;

/// Re-engage the bottom pin when the user returns within this many px of the end.
pub const STICK_THRESHOLD_PX: f32 = 70.0;

/// Show the scroll-to-bottom button beyond this distance from the end.
pub const SCROLL_BUTTON_THRESHOLD_PX: f32 = 320.0;

/// Treat as exactly pinned within this distance of the bottom.
pub const AT_BOTTOM_PX: f32 = 2.0;

/// Bound session-local viewport memory independently of total chat history.
pub const MAX_SAVED_VIEWPORTS: usize = 256;

/// A freshly-sent prompt rests this far below the transcript viewport's top.
#[allow(dead_code)]
pub const OWN_SEND_TOP_INSET_PX: f32 = Theme::TITLEBAR_HEIGHT + 10.0;

/// Epsilon of extra height under the reservation.
#[allow(dead_code)]
pub const OWN_SEND_SCROLL_SLACK_PX: f32 = 2.0;

/// Signed scroll direction: true when a wheel/touch sample is moving toward the
/// bottom. Rune's own viewport uses this to decide whether a small band entry
/// is a re-stick or a wheel-up escape.
#[allow(dead_code)]
pub fn moving_toward_bottom(distance: f32, previous_distance: f32) -> bool {
    distance < previous_distance
}

/// Whether a user scroll should re-engage the bottom pin: inside the 70px stick
/// band *and* moving toward the bottom. Direction matters — a small wheel-up
/// notch near the bottom stays inside the band, and re-sticking on it would
/// snap the view straight back, making the pin unbreakable.
pub fn should_restick(distance: f32, previous_distance: f32) -> bool {
    distance <= STICK_THRESHOLD_PX && distance < previous_distance
}

/// The reservation a held turn still needs: the room under the prompt's
/// top-inset position (`usable` = viewport minus inset and bottom chrome) not
/// yet consumed by the turn's own content.
#[allow(dead_code)]
pub fn own_turn_reservation(usable: f32, turn_height: f32) -> f32 {
    (usable - turn_height).max(0.0)
}

/// Layout state for the most recent locally-sent turn (notes-app parity).
///
/// Rune's `stick_bottom` viewport already follows the growing reply; the
/// anchor is kept so a send can reserve the runway and so a later `Tx`-based
/// `scroll_to` can hold the prompt at its inset. The gpui absolute hold is not
/// reimplemented (see docs/rune-port.md): the end-follow pin owns the view.
#[derive(Clone, Debug)]
pub struct OwnTurnAnchor {
    pub chat_id: String,
    pub message_id: String,
    /// Current reservation pad on the last row (`usable − turn_height`).
    pub runway: f32,
    /// The step still owns the viewport (glide → hold).
    pub held: bool,
    /// The entry glide has landed.
    pub positioned: bool,
    /// Once the prompt has appeared, its later disappearance is terminal.
    pub seen_prompt: bool,
}

impl OwnTurnAnchor {
    pub fn released_for_restore(mut self) -> Self {
        self.held = false;
        self.positioned = false;
        self.seen_prompt = true;
        self
    }

    pub fn observe_prompt(&mut self, exists: bool) -> bool {
        if exists {
            self.seen_prompt = true;
        }
        exists || !self.seen_prompt
    }
}

/// A stable per-chat viewport anchor.
#[derive(Clone, Debug)]
pub struct ViewportAnchor {
    pub row_id: String,
    pub entry_id: String,
    pub fallback_ix: usize,
    pub offset_in_row: f32,
}

impl ViewportAnchor {
    pub fn capture(rows: &[super::model::Row], scroll_top_ix: usize) -> Option<Self> {
        let fallback_ix = scroll_top_ix.min(rows.len().checked_sub(1)?);
        let row = &rows[fallback_ix];
        Some(Self {
            row_id: row.id.clone(),
            entry_id: row.entry_id.clone(),
            fallback_ix,
            offset_in_row: 0.0,
        })
    }

    pub fn resolve(&self, rows: &[super::model::Row]) -> Option<(usize, f32)> {
        if let Some(item_ix) = rows.iter().position(|row| row.id == self.row_id) {
            return Some((item_ix, self.offset_in_row));
        }
        let item_ix = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.entry_id == self.entry_id)
            .min_by_key(|(ix, _)| ix.abs_diff(self.fallback_ix))
            .map(|(ix, _)| ix)
            .unwrap_or_else(|| self.fallback_ix.min(rows.len().saturating_sub(1)));
        (!rows.is_empty()).then_some((item_ix, 0.0))
    }
}

/// Session-local viewport state.
#[derive(Clone, Debug)]
pub enum SavedViewport {
    FollowTail,
    Anchored {
        anchor: ViewportAnchor,
        distance_from_bottom: f32,
        own_turn: Option<OwnTurnAnchor>,
    },
}

impl SavedViewport {
    pub fn capture(
        rows: &[super::model::Row],
        scroll_top_ix: usize,
        pinned: bool,
        distance_from_bottom: f32,
        own_turn: Option<&OwnTurnAnchor>,
    ) -> Option<Self> {
        if rows.is_empty() {
            return None;
        }
        if pinned {
            return Some(Self::FollowTail);
        }
        Some(Self::Anchored {
            anchor: ViewportAnchor::capture(rows, scroll_top_ix)?,
            distance_from_bottom,
            own_turn: own_turn.cloned(),
        })
    }
}

#[derive(Default)]
pub struct SavedViewportCache {
    pub by_chat: HashMap<String, SavedViewport>,
    recency: VecDeque<String>,
}

impl SavedViewportCache {
    pub fn insert(&mut self, chat_id: String, viewport: SavedViewport) {
        if self.by_chat.contains_key(&chat_id) {
            self.recency.retain(|candidate| candidate != &chat_id);
        }
        self.recency.push_back(chat_id.clone());
        self.by_chat.insert(chat_id, viewport);
        while self.by_chat.len() > MAX_SAVED_VIEWPORTS {
            let Some(evicted) = self.recency.pop_front() else {
                break;
            };
            self.by_chat.remove(&evicted);
        }
    }

    pub fn get_cloned_and_touch(&mut self, chat_id: &str) -> Option<SavedViewport> {
        let viewport = self.by_chat.get(chat_id).cloned()?;
        self.recency.retain(|candidate| candidate != chat_id);
        self.recency.push_back(chat_id.to_string());
        Some(viewport)
    }

    pub fn len(&self) -> usize {
        self.by_chat.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_chat.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::model::{Row, RowKind};

    fn row(id: &str, entry: &str) -> Row {
        Row {
            id: id.into(),
            version: 0,
            turn_start: false,
            kind: RowKind::ErrorChip {
                message: String::new(),
            },
            entry_id: entry.into(),
            timestamp: None,
            copy_text: None,
        }
    }

    #[test]
    fn stick_band_is_direction_aware() {
        assert!(should_restick(0.0, 10.0));
        assert!(should_restick(70.0, 200.0));
        assert!(!should_restick(70.1, 200.0));
        // A wheel-up notch inside the band must not re-stick.
        assert!(!should_restick(40.0, 30.0));
        assert!(moving_toward_bottom(10.0, 20.0));
    }

    #[test]
    fn reservation_shrinks_as_the_turn_grows() {
        assert_eq!(own_turn_reservation(600.0, 100.0), 500.0);
        assert_eq!(own_turn_reservation(600.0, 800.0), 0.0);
    }

    #[test]
    fn own_turn_observes_a_prompt_once() {
        let mut anchor = OwnTurnAnchor {
            chat_id: "c".into(),
            message_id: "m".into(),
            runway: 0.0,
            held: true,
            positioned: false,
            seen_prompt: false,
        };
        assert!(anchor.observe_prompt(false));
        assert!(anchor.observe_prompt(true));
        assert!(anchor.observe_prompt(false) == false);
        let restored = anchor.released_for_restore();
        assert!(!restored.held);
        assert!(!restored.positioned);
    }

    #[test]
    fn anchors_resolve_by_id_then_by_entry() {
        let rows = vec![row("a", "e1"), row("b", "e2"), row("c", "e2")];
        let anchor = ViewportAnchor {
            row_id: "b".into(),
            entry_id: "e2".into(),
            fallback_ix: 1,
            offset_in_row: 12.0,
        };
        assert_eq!(anchor.resolve(&rows), Some((1, 12.0)));
        let reshaped = ViewportAnchor {
            row_id: "gone".into(),
            entry_id: "e2".into(),
            fallback_ix: 2,
            offset_in_row: 4.0,
        };
        assert_eq!(reshaped.resolve(&rows), Some((2, 0.0)));
        assert_eq!(ViewportAnchor::capture(&[], 0).is_none(), true);
    }

    #[test]
    fn viewport_cache_is_an_lru_capped_at_256() {
        let mut cache = SavedViewportCache::default();
        for ix in 0..MAX_SAVED_VIEWPORTS + 10 {
            cache.insert(format!("chat{ix}"), SavedViewport::FollowTail);
        }
        assert_eq!(cache.len(), MAX_SAVED_VIEWPORTS);
        assert!(cache.get_cloned_and_touch("chat0").is_none());
        assert!(cache.get_cloned_and_touch("chat20").is_some());
        // Touching refreshes recency.
        cache.insert("fresh".into(), SavedViewport::FollowTail);
        assert!(cache.get_cloned_and_touch("chat20").is_some());
    }
}
