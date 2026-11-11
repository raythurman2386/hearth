//! hearth-ui-rune — the Rune viewport. Shell, sidebar, conversation, composer,
//! terminal and diff pane, ported from the gpui UI.
//!
//! The engine, RPC, doc and sync crates are UI-agnostic and shared unchanged;
//! this crate replaces `crates/ui` (gpui) with a Rune implementation.

pub mod app;
pub mod appearance;
pub mod app_menus;
pub mod attachments;
pub mod badges;
pub mod bridge;
pub mod change_requests;
pub mod changes;
pub mod comments;
pub mod composer;
pub mod edge_fade;
pub mod engine;
pub mod frost;
pub mod history;
pub mod icons;
pub mod loaders;
pub mod markdown;
pub mod motion;
pub mod notify;
pub mod pickers;
pub mod rail;
pub mod settings;
pub mod shell;
pub mod sound;
pub mod state;
pub mod syntax_cache;
pub mod terminal;
pub mod theme;
pub mod transcript;

pub use state::App;

use std::path::PathBuf;

use hearth_proto::HarnessId;

/// Everything the headed binary passes in (config/env resolution lives in
/// `apps/hearth`, not here).
#[derive(Debug, Clone)]
pub struct UiConfig {
    /// Data directory — engine stores + `ui-settings.json`.
    pub data_dir: PathBuf,
    /// Localhost IPC port: connect if an engine daemon is listening, embed if not.
    pub ipc_port: u16,
    /// Edge base URL for the embedded engine.
    pub edge_url: String,
    /// Edge bearer; `None` runs offline.
    pub edge_token: Option<String>,
    /// Workspace org override for explicit dev-mode runs.
    pub org_id: Option<String>,
    /// Harness for doc-command runs until per-chat config lands.
    pub default_harness: HarnessId,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from(".hearth"),
            ipc_port: 27654,
            edge_url: String::new(),
            edge_token: None,
            org_id: None,
            default_harness: HarnessId::Huginn,
        }
    }
}

/// Run the headed app: Rune window, engine bootstrap, shell root.
pub fn run_app(config: UiConfig) {
    let spec = app::spec();
    let app = rune::App::new(spec)
        .init(move || App::new(config))
        .chrome(rune::Chrome {
            full_size_content: true,
            traffic_lights: Some((14.0, 14.0)),
            drag_regions: true,
        })
        .titlebar(
            rune::Titlebar::new(app::titlebar_height()).traffic_lights(14.0, 14.0),
        )
        .on_focus(|app, focused, _tx| {
            shell::on_focus(app, focused);
        })
        .on_close(|app, _tx| {
            // Graceful teardown: drain live runs and flush doc snapshots before
            // the process exits. `on_close` runs on the UI thread, so blocking
            // the bridge runtime here is safe and bounded.
            if let Some(engine) = app.engine.clone() {
                app.bridge.runtime().block_on(async move {
                    let _ = tokio::time::timeout(
                        std::time::Duration::from_secs(2),
                        engine.shutdown(),
                    )
                    .await;
                });
            }
        })
        .standard_menus();
    app_menus::install(app).run();
}
