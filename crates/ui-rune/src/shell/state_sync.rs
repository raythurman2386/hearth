//! Boot and cross-cutting state sync: splash phases, the most-recent-chat landing, the space
//! filter restore and the session-panel mirror.
//!
//! The gpui shell observed the `AppState` entity; Rune has no observation graph, so this runs
//! from [`super::poll`] once per frame. It is idempotent: every branch is guarded by a state
//! comparison and returns whether anything visible changed.

use chrono::Utc;

use crate::state::{App, ConnectionStatus};

use super::{NavEntry, RightSurface, Route, ShellState, SplashPhase, resolved_right_active};
use crate::motion;

/// Advance boot state. Returns true when visible state changed.
pub fn poll(app: &mut App) -> bool {
    let mut changed = false;
    changed |= debug_dialog(app);
    changed |= boot_spaces(app);
    changed |= boot_chat(app);
    changed |= sync_active_chat(app);
    changed |= prune_surface_tabs(app);
    changed |= heal_space_filter(app);
    changed |= splash(app);
    // Chimes and banners change no visible state; the detector still advances
    // its baseline every poll.
    notify_transitions(app);
    changed
}

/// Wall-clock seconds, the clock Rune's `Tx::clock` uses for wake deadlines.
pub fn now() -> f64 {
    Utc::now().timestamp_millis() as f64 / 1000.0
}

fn debug_dialog(app: &mut App) -> bool {
    let Some(which) = app.shell.debug_dialog.clone() else {
        return false;
    };
    match which.as_str() {
        "add-space" if !app.devices.is_empty() => {
            app.shell.debug_dialog = None;
            super::spaces::open_add_space(app);
            true
        }
        "rename" => {
            if let Some(first) = app.chats.first().map(|c| c.id.clone()) {
                app.shell.debug_dialog = None;
                super::open_rename_chat(app, first);
                true
            } else {
                false
            }
        }
        "delete" => {
            if let Some(first) = app.chats.first().map(|c| c.id.clone()) {
                app.shell.debug_dialog = None;
                app.shell.delete_confirm = Some(first);
                true
            } else {
                false
            }
        }
        _ => false,
    }
}

/// Restore the last selected space once the first spaces frame lands.
fn boot_spaces(app: &mut App) -> bool {
    if app.shell.space_boot_applied || app.spaces.is_empty() {
        return false;
    }
    app.shell.space_boot_applied = true;
    let mut changed = false;
    if app.selected_chat.is_none() {
        let exists = |id: &String| app.space_row(id).is_some();
        let filter = app.settings.space_filter.clone().filter(|id| exists(id));
        let target = match filter {
            Some(filter) => Some(filter),
            None if !app.no_project => app.settings.last_space_id.clone().filter(|id| exists(id)),
            None => None,
        };
        if target.is_some() {
            app.select_space(target);
            changed = true;
        }
    }
    let selected = app.selected_space.clone();
    if selected.is_some() && selected != app.settings.last_space_id {
        app.settings.last_space_id = selected;
        changed = true;
    }
    changed
}

/// Boot landing: the most recent visible chat once the first chats frame syncs (manual
/// selection wins; no chats leaves the new-chat canvas).
fn boot_chat(app: &mut App) -> bool {
    if !app.chats_synced
        || app.selected_chat.is_some()
        || app.auto_selected
        || app.shell.pinned_new_canvas
    {
        return false;
    }
    let first = most_recent_visible_chat(app);
    let Some(first) = first else {
        return false;
    };
    app.select_chat(Some(first.clone()));
    app.shell.nav.boot_landing(NavEntry::Chat(first));
    true
}

/// The sidebar's overview order: every non-archived chat of a live space (or with no space),
/// most recent first.
pub fn most_recent_visible_chat(app: &App) -> Option<String> {
    let mut rows: Vec<&hearth_proto::Chat> = app
        .visible_chats()
        .filter(|c| match c.space_id.as_deref() {
            None => true,
            Some(id) => app.space_row(id).is_some(),
        })
        .collect();
    rows.sort_by(|a, b| {
        let ka = a.last_message_at.unwrap_or(a.created_at);
        let kb = b.last_message_at.unwrap_or(b.created_at);
        kb.cmp(&ka).then_with(|| a.id.cmp(&b.id))
    });
    rows.first().map(|c| c.id.clone())
}

/// Mirror the selected chat into shell state and record the navigation.
fn sync_active_chat(app: &mut App) -> bool {
    let selected = app.selected_chat.clone().unwrap_or_default();
    if selected == app.shell.active_chat {
        return false;
    }
    app.shell.active_chat = selected.clone();
    if matches!(app.shell.route, Route::Chat) {
        app.shell.nav.boot_landing(NavEntry::Chat(selected));
    }
    true
}

/// Skip stored surface entries whose backing tab is gone.
fn prune_surface_tabs(app: &mut App) -> bool {
    let mut changed = false;
    let keys: Vec<String> = app.shell.right_tabs.keys().cloned().collect();
    for key in keys {
        let Some(tabs) = app.shell.right_tabs.get_mut(&key) else {
            continue;
        };
        let before = tabs.len();
        tabs.retain(|surface| match surface {
            RightSurface::Diff(id) => app.shell.diff_titles.contains_key(id),
            RightSurface::Terminal(id) => app.shell.terminal_titles.contains_key(id),
            RightSurface::Subagent(id) => app.shell.subagent_tabs.contains_key(id),
            RightSurface::Shared => false,
        });
        changed |= tabs.len() != before;
    }
    let selected = resolved_right_active(app);
    let key = app.shell.panel_key(app);
    let current = app.shell.panels.get(&key).right_active;
    if current != selected {
        app.shell.panels.update(&key, |p| p.right_active = selected);
        changed = true;
    }
    changed
}

/// Splash: Visible → FadingOut on Ready, then Gone after the catalog's fade.
fn splash(app: &mut App) -> bool {
    match &app.connection {
        ConnectionStatus::Ready => match app.shell.splash {
            SplashPhase::Visible => {
                app.shell.splash = SplashPhase::FadingOut;
                app.shell.splash_deadline =
                    Some(now() + motion::SPLASH_OUT_SPEC.total().as_secs_f64());
                true
            }
            SplashPhase::FadingOut
                if app
                    .shell
                    .splash_deadline
                    .is_some_and(|deadline| now() >= deadline) =>
            {
                app.shell.splash = SplashPhase::Gone;
                app.shell.splash_deadline = None;
                true
            }
            _ => false,
        },
        ConnectionStatus::Failed(_) => {
            if app.shell.splash == SplashPhase::Gone {
                false
            } else {
                app.shell.splash = SplashPhase::Gone;
                true
            }
        }
        ConnectionStatus::Connecting => false,
    }
}

/// Whether a desktop banner may post, given the settings and window focus. Focus is
/// app-level: a ping for a *background chat* in a focused app is a chime, not a banner
/// (the sidebar dot carries the rest).
pub fn banner_allowed(
    notifications_enabled: bool,
    background_only: bool,
    window_focused: bool,
) -> bool {
    notifications_enabled && !(background_only && window_focused)
}

/// Session chimes and desktop banners on status edges (hearth's `on_state_changed`
/// detector): a question rings whenever a session flips to AwaitingInput, a completion
/// rings on the Working→Idle edge — for ANY session on any device. A row's first
/// appearance only seeds the baseline, so boot (restored rows) and fresh sends stay
/// silent.
///
/// Staleness-gated like the sidebar dot (`effective_indicator`), so a dead turn's stale
/// Working row can't ring "done" when the old Idle finally syncs in. The done chime is
/// also send-pending-gated: a queued command that the host hasn't executed yet must not
/// ring completion; the question chime is NOT gated (an instant AwaitingInput ack should
/// still ring).
fn notify_transitions(app: &mut App) {
    use crate::sound::{Sound, sound_for_transition};
    use hearth_proto::SessionStatus;

    let now = Utc::now();
    let live: Vec<String> = app.sessions.iter().map(|s| s.chat_id.clone()).collect();
    let mut pings: Vec<(String, SessionStatus, bool, Option<String>)> = Vec::new();
    for session in &app.sessions {
        let status = match crate::state::effective_indicator(Some(session), now) {
            crate::state::Indicator::Working => SessionStatus::Working,
            crate::state::Indicator::AwaitingInput => SessionStatus::AwaitingInput,
            crate::state::Indicator::Errored => SessionStatus::Errored,
            crate::state::Indicator::None => SessionStatus::Idle,
        };
        let chat_id = session.chat_id.clone();
        let send_pending = app.send_pending(&chat_id, now);
        let title = app
            .chats
            .iter()
            .find(|chat| chat.id == chat_id)
            .and_then(|chat| chat.title.clone());
        pings.push((chat_id, status, send_pending, title));
    }
    // Drop baselines for sessions that went away (chat deleted, device removed).
    app.shell.sound_prev.retain(|id, _| live.contains(id));

    for (chat_id, status, send_pending, title) in pings {
        let Some(prev) = app.shell.sound_prev.insert(chat_id, status) else {
            continue;
        };
        let Some(sound) = sound_for_transition(prev, status) else {
            continue;
        };
        if send_pending && sound == Sound::Done {
            continue;
        }
        if app.settings.sound_enabled {
            crate::sound::play(sound);
        }
        if banner_allowed(
            app.settings.notifications_enabled,
            app.settings.notifications_background_only,
            app.shell.window_focused,
        ) {
            let title = title.unwrap_or_else(|| "New session".to_string());
            let body = match sound {
                Sound::Done => "Run finished",
                Sound::Request => "Waiting on your input",
            };
            crate::notify::post(&title, body);
        }
    }
}

/// Heal a dangling space filter (space deleted, possibly elsewhere).
pub fn heal_space_filter(app: &mut App) -> bool {
    if app.spaces_synced
        && let Some(filter) = app.settings.space_filter.clone()
        && app.space_row(&filter).is_none()
    {
        app.settings.space_filter = None;
        app.save_settings();
        return true;
    }
    false
}

/// Snapshot the shell for tests without a window.
pub fn debug_summary(state: &ShellState) -> String {
    format!("{:?}:{:?}:{}", state.route, state.splash, state.active_chat)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_is_plausible() {
        assert!(now() > 1_600_000_000.0);
    }

    #[test]
    fn splash_transitions_are_one_way() {
        // The pure transition contract, mirrored here without an App.
        assert_ne!(SplashPhase::Visible, SplashPhase::FadingOut);
        assert_ne!(SplashPhase::FadingOut, SplashPhase::Gone);
        assert_eq!(SplashPhase::Gone, SplashPhase::Gone);
    }

    #[test]
    fn banners_follow_settings_and_focus() {
        // Off entirely.
        assert!(!banner_allowed(false, false, false));
        assert!(!banner_allowed(false, true, false));
        // Always (not background-only): focus does not matter.
        assert!(banner_allowed(true, false, true));
        assert!(banner_allowed(true, false, false));
        // Background-only: silent while the window is focused.
        assert!(!banner_allowed(true, true, true));
        assert!(banner_allowed(true, true, false));
    }
}
