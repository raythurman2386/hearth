//! Settings → Agents: enable/disable harnesses — one card row per agent with a
//! trailing switch.
//!
//! The state is PER-DEVICE and lives on the engine (`harness-prefs.json` in its
//! data dir): CLI installs are per-device, so enablement is too. The page-header
//! device switcher retargets both the `ListHarnesses` probe and the
//! `SetHarnessEnabled` writes at any registered device over the
//! relay-forwarded RPCs.
//!
//! Enablement follows DETECTION: every harness whose CLI probe passes is on
//! unless the user switched it off. A harness whose CLI is missing on the
//! target device renders dimmed with an install hint and is never enabled; an
//! ENABLED agent can always be turned OFF except the last one standing.

use std::cell::Cell;
use std::sync::{Arc, Mutex};

use hearth_engine::registry::{HarnessDescriptor, descriptor_enabled};
use hearth_proto::HarnessId;
use hearth_rpc::methods;
use rune::{El, TextStyle, Tx, col, el, row, text};
use rune_kit::Placement;

use crate::icons;
use crate::pickers::Loadable;
use crate::pickers::model::harness_mark;
use crate::state::App;
use crate::theme::Theme;

use super::widgets;

/// The page-header device switcher's popover key.
const DEVICE_SWITCHER: &str = "settings/harnesses-device";

/// One-line blurb per agent.
pub fn blurb(harness: HarnessId) -> &'static str {
    match harness {
        HarnessId::Codex => "OpenAI's coding agent, driven through the Codex CLI.",
        HarnessId::Grok => "xAI's Grok Build agent (grok CLI).",
        HarnessId::Huginn => "A local-first coding-agent harness (huginn CLI).",
        HarnessId::Mock => "Scripted test harness.",
    }
}

/// The CLI named in the not-installed hint.
pub fn cli_name(harness: HarnessId) -> &'static str {
    match harness {
        HarnessId::Codex => "codex",
        HarnessId::Grok => "grok",
        HarnessId::Huginn => "huginn",
        HarnessId::Mock => "mock",
    }
}

/// Merge the `targetDeviceId` passthrough into a params object. Pure.
pub fn with_target(mut value: serde_json::Value, target: Option<&str>) -> serde_json::Value {
    if let (Some(target), Some(object)) = (target, value.as_object_mut()) {
        object.insert("targetDeviceId".into(), serde_json::json!(target));
    }
    value
}

type HarnessSlot = Arc<Mutex<Option<(u64, Result<Vec<HarnessDescriptor>, String>)>>>;

/// View state for the Agents (harnesses) page.
#[derive(Debug, Default)]
pub struct HarnessesPage {
    pub harnesses: Loadable<Vec<HarnessDescriptor>>,
    /// Which device's harnesses are shown/edited; `None` = this device.
    pub target_device: Option<String>,
    pub device_menu_open: bool,
    /// Harness with an in-flight toggle.
    pub busy: Option<HarnessId>,
    pub error: Option<String>,
    load_rx: HarnessSlot,
    toggle_rx: HarnessSlot,
    generation: Cell<u64>,
    started: Cell<bool>,
}

impl HarnessesPage {
    /// First-render load. Rendering may not mutate through `&Page`, so this
    /// records the start and posts the result to `poll`.
    pub fn ensure_loaded(&self, app: &App) {
        if self.started.get() || app.engine.is_none() {
            return;
        }
        self.started.set(true);
        let generation = self.next_generation();
        begin_load(
            app,
            self.load_rx.clone(),
            self.target_device.clone(),
            generation,
        );
    }

    fn next_generation(&self) -> u64 {
        let next = self.generation.get().wrapping_add(1);
        self.generation.set(next);
        next
    }

    /// Drain a finished list/toggle.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        if let Some((generation, result)) = self.load_rx.lock().unwrap().take()
            && generation == self.generation.get()
        {
            self.harnesses = loadable(result);
            changed = true;
        }
        if let Some((generation, result)) = self.toggle_rx.lock().unwrap().take()
            && generation == self.generation.get()
        {
            self.busy = None;
            match result {
                Ok(list) => {
                    self.harnesses = Loadable::Ready(list);
                    self.error = None;
                }
                Err(message) => self.error = Some(message),
            }
            changed = true;
        }
        changed
    }
}

fn loadable(result: Result<Vec<HarnessDescriptor>, String>) -> Loadable<Vec<HarnessDescriptor>> {
    match result {
        Ok(list) => Loadable::Ready(list),
        Err(message) => Loadable::Error(message),
    }
}

fn begin_load(app: &App, slot: HarnessSlot, target: Option<String>, generation: u64) {
    let Some(client) = app.engine.as_ref().map(|engine| engine.client.clone()) else {
        *slot.lock().unwrap() = Some((generation, Err("Engine not connected".to_string())));
        return;
    };
    let bridge = app.bridge.clone();
    let params = with_target(serde_json::json!({}), target.as_deref());
    app.bridge.spawn(async move {
        let result = client
            .call(methods::LIST_HARNESSES, params)
            .await
            .map_err(|err| err.to_string())
            .and_then(|value| {
                serde_json::from_value::<Vec<HarnessDescriptor>>(value)
                    .map_err(|err| err.to_string())
            });
        *slot.lock().unwrap() = Some((generation, result));
        bridge.wake();
    });
}

/// Reload the visible device's catalog.
pub fn reload(app: &mut App) {
    let page = &mut app.settings_ui.harnesses;
    page.harnesses = Loadable::Loading;
    page.error = None;
    page.started.set(true);
    let generation = page.next_generation();
    let slot = page.load_rx.clone();
    let target = page.target_device.clone();
    begin_load(app, slot, target, generation);
}

/// Retarget the page at another device and reload.
pub fn set_target_device(app: &mut App, target: Option<String>) {
    let page = &mut app.settings_ui.harnesses;
    page.device_menu_open = false;
    if page.target_device == target {
        return;
    }
    page.target_device = target;
    page.harnesses = Loadable::Loading;
    page.error = None;
    page.busy = None;
    page.started.set(true);
    let generation = page.next_generation();
    let slot = page.load_rx.clone();
    let target = page.target_device.clone();
    begin_load(app, slot, target, generation);
}

/// Flip one harness's enablement on the target device.
pub fn toggle_harness(app: &mut App, harness: HarnessId, enabled: bool) {
    let page = &mut app.settings_ui.harnesses;
    page.error = None;
    page.busy = Some(harness);
    let generation = page.next_generation();
    let slot = page.toggle_rx.clone();
    let target = page.target_device.clone();
    let Some(client) = app.engine.as_ref().map(|engine| engine.client.clone()) else {
        page.busy = None;
        page.error = Some("Engine not connected".to_string());
        return;
    };
    let bridge = app.bridge.clone();
    let params = with_target(
        serde_json::json!({
            "harness": harness,
            "enabled": enabled,
        }),
        target.as_deref(),
    );
    app.bridge.spawn(async move {
        let result = client
            .call(methods::SET_HARNESS_ENABLED, params)
            .await
            .map_err(|err| err.to_string())
            .and_then(|value| {
                serde_json::from_value::<Vec<HarnessDescriptor>>(value)
                    .map_err(|err| err.to_string())
            });
        *slot.lock().unwrap() = Some((generation, result));
        bridge.wake();
    });
    // The composer caches its catalog per space: drop it so the rail refetches
    // the new enabled set (the gpui page bumped after the reply; the handler is
    // the only place that can reach `App`).
    crate::pickers::bump_harness_catalog(app);
}

fn open_device_menu(app: &mut App) {
    let page = &mut app.settings_ui.harnesses;
    page.device_menu_open = !page.device_menu_open;
}

fn close_device_menu(app: &mut App) {
    app.settings_ui.harnesses.device_menu_open = false;
}

fn platform_glyph(platform: &str) -> &'static rune::Icon {
    match platform {
        "macos" | "darwin" => &icons::LAPTOP,
        "ios" | "android" => &icons::SMARTPHONE,
        _ => &icons::MONITOR,
    }
}

fn device_switcher(page: &HarnessesPage, app: &App, theme: &Theme, layer: &mut Vec<El>) -> El {
    let mut devices = app.devices.clone();
    devices.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    let local_id = app.local_device_id.clone();
    let effective = page.target_device.clone().or_else(|| local_id.clone());
    let selected = devices
        .iter()
        .find(|d| Some(d.id.as_str()) == effective.as_deref());
    let trigger_glyph = platform_glyph(selected.map(|d| d.platform.as_str()).unwrap_or("macos"));
    let trigger_label = selected
        .map(|d| d.name.clone())
        .unwrap_or_else(|| "This device".to_string());
    let open = page.device_menu_open;

    let trigger = row()
        .h(28.0)
        .px(8.0)
        .radius(6.0)
        .gap(6.0)
        .center_y()
        .cursor(rune::Cursor::Pointer)
        .fill(if open {
            theme.ink(0.06)
        } else {
            theme.ink(0.0)
        })
        .hover(theme.ink(0.04))
        .on_press(|app: &mut App| open_device_menu(app))
        .child(icons::icon(trigger_glyph, 16.0).color(theme.text_muted))
        .child(
            text(trigger_label)
                .min_w(0.0)
                .truncate()
                .font(TextStyle::new(12.5, 500.0))
                .color(theme.text),
        )
        .child(
            el().w(6.0)
                .h(6.0)
                .radius(3.0)
                .fill(if effective == local_id {
                    theme.success
                } else {
                    theme.ink(0.2)
                }),
        )
        .child(
            icons::icon(&icons::SORT_VERTICAL, 14.0).color(theme.text_muted.alpha(if open {
                0.9
            } else {
                0.4
            })),
        );

    let mut body = col().w(220.0).gap(2.0).child(
        text("Devices")
            .font(TextStyle::new(11.0, 500.0))
            .color(theme.text_muted.alpha(0.6))
            .px(8.0)
            .py(4.0),
    );
    for (ix, device) in devices.iter().enumerate() {
        let is_active = Some(device.id.as_str()) == effective.as_deref();
        let is_local = local_id.as_deref() == Some(device.id.as_str());
        let glyph = platform_glyph(&device.platform);
        let pick_id = device.id.clone();
        let pick_local = is_local;
        let mut item = row()
            .key(format!("{DEVICE_SWITCHER}-row-{ix}"))
            .w_full()
            .h(30.0)
            .px(8.0)
            .radius(6.0)
            .gap(8.0)
            .center_y()
            .cursor(rune::Cursor::Pointer)
            .fill(if is_active {
                theme.ink(0.06)
            } else {
                theme.ink(0.0)
            })
            .hover(theme.ink(0.06))
            .on_press(move |app: &mut App| {
                set_target_device(app, (!pick_local).then(|| pick_id.clone()));
            })
            .child(icons::icon(glyph, 16.0).color(theme.text_muted))
            .child(
                text(device.name.clone())
                    .grow(1.0)
                    .min_w(0.0)
                    .truncate()
                    .font(TextStyle::new(13.0, 400.0))
                    .color(theme.text),
            );
        if is_local {
            item = item.child(
                text("You")
                    .font(TextStyle::new(10.5, 400.0))
                    .color(theme.text_muted.alpha(0.35)),
            );
        }
        item = item.child(el().w(6.0).h(6.0).radius(3.0).fill(if is_local {
            theme.success
        } else {
            theme.ink(0.2)
        }));
        body = body.child(item);
    }

    rune_kit::Popover::new(DEVICE_SWITCHER)
        .trigger(trigger)
        .open(open)
        .placement(Placement::BottomEnd)
        .padding(4.0)
        .on_close(|app: &mut App| close_device_menu(app))
        .child(body)
        .build(&crate::shell::shell_kit(theme), layer)
}

fn harness_row(
    page: &HarnessesPage,
    ix: usize,
    descriptor: &HarnessDescriptor,
    enabled_count: usize,
    theme: &Theme,
) -> El {
    let harness = descriptor.id;
    let installed = descriptor.installed;
    let enabled = descriptor_enabled(descriptor);
    // The last enabled harness whose CLI exists can't be switched off — the
    // composer needs something to run. An uninstalled last harness stays
    // togglable so the user can turn the stale row off.
    let last_enabled = enabled && enabled_count == 1 && installed;
    let interactive = !last_enabled && (enabled || installed);
    let busy = page.busy == Some(harness);

    let mut meta = vec![widgets::meta_text(theme, blurb(harness))];
    if !installed {
        meta.push(
            text(if enabled {
                format!(
                    "{} CLI not installed — turn it off or install it",
                    cli_name(harness)
                )
            } else {
                format!("Install the {} CLI to enable", cli_name(harness))
            })
            .font(TextStyle::new(12.0, 400.0))
            .color(theme.warning_muted.alpha(0.9))
            .wrap(),
        );
    }

    let switch = rune_kit::Switch::new(enabled)
        .label(descriptor.name.clone())
        .hide_label()
        .on_toggle(move |app: &mut App| toggle_harness(app, harness, !enabled))
        .disabled(!interactive)
        .build(&crate::shell::shell_kit(theme));

    widgets::card_row(theme, ix == 0)
        .key(format!("harness-row-{ix}"))
        .opacity(if installed { 1.0 } else { 0.55 })
        .child(widgets::row_tile_text(theme, harness_mark(harness)))
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .child(widgets::row_title(theme, descriptor.name.clone()))
                .child(widgets::meta_line(theme, meta)),
        )
        .child(el().opacity(if busy { 0.5 } else { 1.0 }).child(switch))
}

/// The Agents page body.
pub fn render(page: &HarnessesPage, app: &App, theme: &Theme) -> El {
    page.ensure_loaded(app);

    let mut layer: Vec<El> = Vec::new();
    let switcher = device_switcher(page, app, theme, &mut layer);

    let body: El = match &page.harnesses {
        Loadable::Idle | Loadable::Loading => widgets::skeleton_card(theme, 4),
        Loadable::Error(message) => col()
            .w_full()
            .child(widgets::error_strip(theme, message.clone()))
            .child(
                widgets::ghost_action(theme)
                    .mt(8.0)
                    .hover(theme.ink(0.06))
                    .on_press(|app: &mut App| reload(app))
                    .child(
                        text("Retry")
                            .font(TextStyle::new(12.0, 400.0))
                            .color(theme.text_muted),
                    ),
            ),
        Loadable::Ready(list) => {
            let descriptors = crate::pickers::visible_harnesses(list);
            let enabled_count = descriptors.iter().filter(|d| descriptor_enabled(d)).count();
            let mut card = widgets::section_card(theme);
            if descriptors.is_empty() {
                card = card.child(
                    col().w_full().py(40.0).items_center().child(
                        text("No agents available on this device")
                            .font(TextStyle::new(14.0, 400.0))
                            .color(theme.text_muted.alpha(0.6)),
                    ),
                );
            } else {
                for (ix, descriptor) in descriptors.iter().enumerate() {
                    card = card.child(harness_row(page, ix, descriptor, enabled_count, theme));
                }
            }
            card
        }
    };

    let mut content = widgets::page_column()
        .child(
            row()
                .w_full()
                .gap(10.0)
                .center_y()
                .child(widgets::page_header(theme, "Agents", None))
                .child(el().grow(1.0))
                .child(switcher),
        )
        .child(
            widgets::page_subtitle(
                theme,
                "Choose which coding agents the composer offers. The setting is per device — \
                 switch devices in the header. Agents whose CLI isn't installed on a device \
                 can't be enabled there.",
            )
            .max_w(512.0),
        );
    if let Some(message) = page.error.clone() {
        content = content.child(widgets::error_strip(theme, message));
    }
    content = content.child(body);

    rune_kit::root(widgets::page_root("harnesses-page").child(content), layer)
}

/// No page-level key handling.
pub fn on_key(_app: &mut App, _ev: &rune::KeyEvent, _tx: &mut Tx) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_harness_has_copy() {
        for harness in [
            HarnessId::Codex,
            HarnessId::Grok,
            HarnessId::Huginn,
            HarnessId::Mock,
        ] {
            assert!(!blurb(harness).is_empty());
            assert!(!cli_name(harness).is_empty());
        }
    }

    #[test]
    fn target_device_is_merged_only_when_present() {
        let direct = with_target(serde_json::json!({ "enabled": true }), None);
        assert!(direct.get("targetDeviceId").is_none());

        let relayed = with_target(serde_json::json!({ "enabled": true }), Some("dev-2"));
        assert_eq!(
            relayed.get("targetDeviceId").and_then(|v| v.as_str()),
            Some("dev-2")
        );
    }
}
