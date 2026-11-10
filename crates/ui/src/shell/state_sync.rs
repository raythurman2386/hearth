use super::{NavEntry, RightSurface, Route, Shell, SplashPhase};
use crate::motion::SPLASH_OUT;
use crate::state::{AppState, ConnectionStatus};
use chrono::Utc;
use gpui::{Context, Entity};
use std::time::Duration;

impl Shell {
    // ---- splash ----

    pub(super) fn on_state_changed(&mut self, state: &Entity<AppState>, cx: &mut Context<Self>) {
        // Capture knob: the add-space palette needs only the device registry.
        if self.debug_dialog.as_deref() == Some("add-space") && !state.read(cx).devices.is_empty() {
            self.debug_dialog = None;
            self.open_add_space(cx);
        }
        // Capture knob: pop the requested dialog once chats have landed.
        if let Some(which) = self.debug_dialog.clone()
            && let Some(first) = state.read(cx).chats.first().map(|c| c.id.clone())
        {
            self.debug_dialog = None;
            match which.as_str() {
                "rename" => self.open_rename_chat(first, cx),
                "delete" => {
                    self.delete_confirm = Some(first);
                }
                _ => {}
            }
        }
        // Capture knob: `HEARTH_DEMO_UPLOAD=<pct>:<image path>` — once a chat
        // is selected, push a fake sending echo carrying that image as a
        // pending attachment and freeze upload progress at <pct>, so the
        // thumbnail progress ring can be styled/screenshotted (a real upload
        // is too fast to pause).
        if let Some(spec) = self.debug_upload.clone()
            && let Some(chat_id) = state.read(cx).selected_chat.clone()
        {
            self.debug_upload = None;
            if let Some((pct, img_path)) = spec.split_once(':')
                && let Ok(pct) = pct.parse::<u64>()
                && let Ok(att) = crate::attachments::stage_file(std::path::Path::new(img_path))
            {
                let pending_path = format!("pending/{}/{}", att.id, att.name);
                let device_ids: Vec<String> = {
                    let s = state.read(cx);
                    s.selected_chat_row()
                        .map(|c| c.device_id.clone())
                        .into_iter()
                        .chain(s.local_device_id.clone())
                        .chain(Some("local".to_string()))
                        .collect()
                };
                for device_id in &device_ids {
                    crate::attachments::seed_attachment(
                        device_id,
                        &pending_path,
                        &att.name,
                        att.image.clone(),
                    );
                }
                let text = crate::attachments::with_attachments(
                    "Here is the screenshot of the bug.",
                    std::slice::from_ref(&pending_path),
                );
                let echo = hearth_doc::SessionMessageEntry {
                    id: "demo-upload-echo".into(),
                    role: hearth_doc::MessageRole::User,
                    parts: vec![hearth_doc::MessagePart::Text {
                        id: "t0".into(),
                        text,
                    }],
                    created_at: chrono::Utc::now().timestamp_millis(),
                    device_id: "local".into(),
                    status: None,
                    continuation_of: None,
                };
                state.update(cx, |s, cx| {
                    s.push_echo(&chat_id, echo);
                    s.begin_upload_progress(
                        100,
                        std::sync::Arc::new(std::sync::atomic::AtomicU64::new(pct)),
                    );
                    cx.notify();
                });
            }
        }
        // Session chimes (herdr semantics, `sound::sound_for_transition`): a
        // question rings whenever a session flips to AwaitingInput, a
        // completion rings on the Working→Idle edge — for ANY session on any
        // device. A row's first appearance only seeds the baseline, so boot
        // (restored rows) and fresh sends stay silent. Desktop banners
        // (`notify::post`) ride the SAME edges and gates behind their own
        // settings flag — one detector, two outputs, so the banner can never
        // fire where the chime wouldn't.
        //
        // STALENESS-GATED like the dot (`effective_indicator`), for the same
        // reason: raw row statuses include the past. A dead turn's Working row
        // (host killed mid-run, Idle write lost to a wedged room) seeded
        // prev=Working here, and the moment the old Idle finally synced in —
        // typically piggybacked on the round-trip of a fresh send — the chime
        // heard a phantom Working→Idle and rang "done" on send (user report
        // 2026-07-31). The dot never showed that ghost; the chime must judge
        // by the identical clock.
        //
        // SEND-PENDING-GATED too (`AppState::send_pending`): a send whose
        // queued command the host hasn't executed yet can still surface a
        // phantom Working→Idle (a stale Working row crossing the 45s gate on
        // the send's own re-render, or a late old Idle row) — the done-chime
        // stays quiet for that chat until the host acks, while the baseline
        // keeps tracking silently so the ghost edge never fires later. The
        // question chime is NOT gated: an instant AwaitingInput ack should
        // still ring.
        {
            let now = Utc::now();
            type Ping = (String, hearth_proto::SessionStatus, bool, Option<String>);
            let sessions: Vec<Ping> = {
                let state = state.read(cx);
                state
                    .sessions
                    .iter()
                    .map(|s| {
                        use hearth_proto::view::Indicator;
                        let status = match hearth_proto::view::effective_indicator(Some(s), now) {
                            Indicator::Working => hearth_proto::SessionStatus::Working,
                            Indicator::AwaitingInput => hearth_proto::SessionStatus::AwaitingInput,
                            Indicator::Errored => hearth_proto::SessionStatus::Errored,
                            Indicator::None => hearth_proto::SessionStatus::Idle,
                        };
                        let send_pending = state.send_pending(&s.chat_id, now);
                        let title = state
                            .chats
                            .iter()
                            .find(|c| c.id == s.chat_id)
                            .and_then(|c| c.title.clone());
                        (s.chat_id.clone(), status, send_pending, title)
                    })
                    .collect()
            };
            // Background-only banners: `active_window()` is app-level (any
            // Hearth window being key), so a ping for a *background chat* in a
            // focused app still stays a chime — you're already looking at
            // Hearth; the sidebar dot carries the rest.
            let app_focused = cx.active_window().is_some();
            for (chat_id, status, send_pending, title) in sessions {
                let prev = self.sound_prev.insert(chat_id, status);
                if let Some(prev) = prev
                    && let Some(sound) = crate::sound::sound_for_transition(prev, status)
                    && !(send_pending && sound == crate::sound::Sound::Done)
                {
                    if self.settings.sound_enabled {
                        crate::sound::play(sound);
                    }
                    if self.settings.notifications_enabled
                        && !(self.settings.notifications_background_only && app_focused)
                    {
                        let title = title.unwrap_or_else(|| "New session".into());
                        let body = match sound {
                            crate::sound::Sound::Done => "Run finished",
                            crate::sound::Sound::Request => "Waiting on your input",
                        };
                        crate::notify::post(&title, body);
                    }
                }
            }
        }
        // Boot: restore the last selected space once the first spaces frame
        // lands (a still-existing row wins over the auto-selected first one;
        // the boot-auto-selected chat's own space wins over both — selecting a
        // chat implies its space, which `select_chat` already applied).
        if !self.space_boot_applied && !state.read(cx).spaces.is_empty() {
            self.space_boot_applied = true;
            if state.read(cx).selected_chat.is_none() {
                // A set sidebar filter is an explicit standing choice — the
                // canvas defaults (project AND its device) follow it, even
                // over a remembered "no project" opt-out. Otherwise the last
                // selected project stands, unless opted out.
                let exists = |id: &String| state.read(cx).space_row(id).is_some();
                let filter = self.settings.space_filter.clone().filter(|id| exists(id));
                let target = match filter {
                    Some(filter) => Some(filter),
                    None if !state.read(cx).no_project => {
                        self.settings.last_space_id.clone().filter(|id| exists(id))
                    }
                    None => None,
                };
                if target.is_some() {
                    state.update(cx, |s, cx| s.select_space(target, cx));
                }
            }
        }
        // Persist the selected space (the new-tab fallback under "All").
        {
            let selected_space = state.read(cx).selected_space.clone();
            if selected_space != self.settings.last_space_id && selected_space.is_some() {
                self.settings.last_space_id = selected_space;
                self.schedule_save(cx);
            }
        }
        // Boot landing: the most recent session once the first chats frame
        // syncs (manual selection wins).
        self.boot_select_chat(cx);
        // Heal a dangling sidebar filter (space deleted, possibly elsewhere):
        // fall back to "All" rather than filtering everything out.
        if state.read(cx).spaces_synced
            && let Some(filter) = self.settings.space_filter.clone()
            && state.read(cx).space_row(&filter).is_none()
        {
            self.settings.space_filter = None;
            self.schedule_save(cx);
        }
        // Chat switch: restore THAT chat's panel state (per-session open flags;
        // snap, no tween — the panels belong to the destination chat).
        let selected = state.read(cx).selected_chat.clone().unwrap_or_default();
        if selected != self.active_chat {
            self.active_chat = selected;
            // Route history: a chat switch is a navigation. The very first
            // selection off the untouched boot canvas REPLACES that entry —
            // hearth's `/` route redirected into the last-used chat, leaving no
            // dead Back target. Walking history lands here too, but the
            // destination already equals `current()`, so the push dedups.
            if matches!(self.route, Route::Chat) {
                let entry = NavEntry::Chat(self.active_chat.clone());
                if self.nav.len() == 1 && *self.nav.current() == NavEntry::Chat(String::new()) {
                    self.nav.replace(entry);
                } else {
                    self.nav.push(entry);
                }
            }
            self.right_tween = None;
            self.right_takeover_content_tween = None;
            self.main_takeover_tween = None;
            self.terminal_tween = None;
            let panels = self.panels.get(&self.panel_key(cx));
            if let Some(panel) = self.terminal.clone() {
                panel.update(cx, |panel, cx| panel.set_open(panels.terminal_open, cx));
            }
            if panels.changes_open
                && let RightSurface::Diff(id) = self.resolved_right_active(cx)
                && let Some(changes) = self.diffs.get(&id).cloned()
            {
                changes.update(cx, |changes, cx| changes.ensure_content(cx));
            }
        }
        match state.read(cx).connection {
            ConnectionStatus::Ready => {
                if self.splash == SplashPhase::Visible {
                    self.splash = SplashPhase::FadingOut;
                    self.splash_task = Some(cx.spawn(async move |this, cx| {
                        cx.background_executor()
                            .timer(SPLASH_OUT.total() + Duration::from_millis(30))
                            .await;
                        this.update(cx, |shell, cx| {
                            shell.splash = SplashPhase::Gone;
                            cx.notify();
                        })
                        .ok();
                    }));
                }
            }
            // Reveal the gate card immediately; the splash never returns mid-session.
            ConnectionStatus::Failed(_) => self.splash = SplashPhase::Gone,
            ConnectionStatus::Connecting => {}
        }
    }

    pub(super) fn retry_engine(&mut self, cx: &mut Context<Self>) {
        AppState::bootstrap(self.state.clone(), self.boot.clone(), cx);
    }
}
