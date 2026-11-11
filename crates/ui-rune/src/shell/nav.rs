//! Browser-style route history for the titlebar back/forward buttons (hearth
//! window-controls.tsx semantics): every route change pushes an entry; Back/Forward walk the
//! stack without changing it; pushing while behind the tip truncates the entries ahead.

use crate::settings::SettingsSection;

/// One route-history entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavEntry {
    /// A chat route; the id of the selected chat ("" = the new-chat canvas).
    Chat(String),
    Settings(SettingsSection),
}

/// Browser-style navigation history.
#[derive(Debug, Clone)]
pub struct NavHistory {
    entries: Vec<NavEntry>,
    index: usize,
}

impl NavHistory {
    pub fn new(initial: NavEntry) -> Self {
        Self {
            entries: vec![initial],
            index: 0,
        }
    }

    pub fn current(&self) -> &NavEntry {
        &self.entries[self.index]
    }

    /// Record a route change. Re-navigating to the current route is a no-op (selecting the
    /// already-selected chat never happened as a navigation); otherwise any forward branch is
    /// truncated and the entry appended.
    pub fn push(&mut self, entry: NavEntry) {
        if *self.current() == entry {
            return;
        }
        self.entries.truncate(self.index + 1);
        self.entries.push(entry);
        self.index += 1;
    }

    /// Swap the current entry in place without growing the stack (a `replace: true`
    /// navigation — the boot redirect from `/` into the last-used chat leaves no dead Back
    /// target behind).
    pub fn replace(&mut self, entry: NavEntry) {
        self.entries[self.index] = entry;
    }

    /// Replace the current entry when the stack is still the untouched boot canvas; otherwise
    /// push. The boot landing helper.
    pub fn boot_landing(&mut self, entry: NavEntry) {
        if self.len() == 1 && *self.current() == NavEntry::Chat(String::new()) {
            self.replace(entry);
        } else {
            self.push(entry);
        }
    }

    pub fn can_back(&self) -> bool {
        self.index > 0
    }

    /// Memory history keeps every entry, so "behind the last entry" is exactly "can go
    /// forward".
    pub fn can_forward(&self) -> bool {
        self.index + 1 < self.entries.len()
    }

    pub fn back(&mut self) -> Option<NavEntry> {
        if !self.can_back() {
            return None;
        }
        self.index -= 1;
        Some(self.current().clone())
    }

    pub fn forward(&mut self) -> Option<NavEntry> {
        if !self.can_forward() {
            return None;
        }
        self.index += 1;
        Some(self.current().clone())
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn index(&self) -> usize {
        self.index
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat(id: &str) -> NavEntry {
        NavEntry::Chat(id.to_string())
    }

    #[test]
    fn starts_with_nothing_to_walk() {
        let nav = NavHistory::new(chat(""));
        assert!(!nav.can_back());
        assert!(!nav.can_forward());
        assert_eq!(*nav.current(), chat(""));
    }

    #[test]
    fn push_then_back_and_forward() {
        let mut nav = NavHistory::new(chat("a"));
        nav.push(chat("b"));
        nav.push(NavEntry::Settings(SettingsSection::Devices));
        assert!(nav.can_back());
        assert!(!nav.can_forward());
        assert_eq!(nav.back(), Some(chat("b")));
        assert_eq!(nav.back(), Some(chat("a")));
        assert!(!nav.can_back());
        assert!(nav.can_forward());
        assert_eq!(nav.back(), None);
        assert_eq!(nav.forward(), Some(chat("b")));
        assert_eq!(
            nav.forward(),
            Some(NavEntry::Settings(SettingsSection::Devices))
        );
        assert_eq!(nav.forward(), None);
    }

    #[test]
    fn push_dedups_the_current_route() {
        let mut nav = NavHistory::new(chat("a"));
        nav.push(chat("a"));
        nav.push(chat("a"));
        assert_eq!(nav.len(), 1);
        nav.push(NavEntry::Settings(SettingsSection::Agents));
        nav.push(NavEntry::Settings(SettingsSection::Agents));
        assert_eq!(nav.len(), 2);
    }

    #[test]
    fn push_truncates_the_forward_branch() {
        let mut nav = NavHistory::new(chat("a"));
        nav.push(chat("b"));
        nav.push(chat("c"));
        nav.back();
        nav.back();
        assert_eq!(*nav.current(), chat("a"));
        assert!(nav.can_forward());
        nav.push(chat("d"));
        assert!(!nav.can_forward());
        assert_eq!(nav.len(), 2);
        assert_eq!(nav.back(), Some(chat("a")));
        assert_eq!(nav.forward(), Some(chat("d")));
    }

    #[test]
    fn replace_swaps_in_place() {
        let mut nav = NavHistory::new(chat(""));
        nav.replace(chat("boot"));
        assert_eq!(nav.len(), 1);
        assert_eq!(*nav.current(), chat("boot"));
        assert!(!nav.can_back());
    }

    #[test]
    fn boot_landing_replaces_only_the_untouched_canvas() {
        let mut nav = NavHistory::new(chat(""));
        nav.boot_landing(chat("boot"));
        assert_eq!(nav.len(), 1);
        assert!(!nav.can_back());

        let mut nav = NavHistory::new(chat("a"));
        nav.boot_landing(chat("b"));
        assert_eq!(nav.len(), 2);
    }

    #[test]
    fn settings_sections_are_distinct_entries() {
        let mut nav = NavHistory::new(chat("a"));
        nav.push(NavEntry::Settings(SettingsSection::Devices));
        nav.push(NavEntry::Settings(SettingsSection::Shortcuts));
        assert_eq!(nav.len(), 3);
        assert_eq!(
            nav.back(),
            Some(NavEntry::Settings(SettingsSection::Devices))
        );
        assert_eq!(nav.back(), Some(chat("a")));
    }
}
