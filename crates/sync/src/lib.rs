//! hearth-sync — room clients (chat2 + registry over WebSocket), the local
//! `DocsStore` (SQLite snapshots + processed-command ledger), and the tailnet
//! hub that replaces the Cloudflare Worker + Durable Objects.
//!
//! - [`ChatClient`]: joins a chat2 room (`ws://hub/chat2/{chatId}/ws`),
//!   catches up via checkpoint + row backfill, pushes local loro updates as
//!   rows, and reconnects with exponential backoff.
//! - [`RegistryClient`]: the per-profile workspace registry room (sidebar rows,
//!   presence).
//! - [`Hub`]: HTTP + WebSocket server spoken by the always-on tailnet host.
//! - [`DocsStore`]: snapshot persistence (the doc IS the outbox — commands + user entries
//!   flush immediately) and the processed-command ledger with mark-BEFORE-execute semantics.

pub mod chat_client;
pub mod chat_frames;
pub mod chat_room;
pub mod dial;
pub mod hub;
pub mod net_path;
pub mod registry;
pub mod registry_room;
mod store;
pub mod tailnet;
mod types;
pub mod wake;

pub use chat_client::{
    ChatClient, ChatDocSink, ChatEvent, ChatStatsSnapshot, ChatTuning, CheckpointFetcher,
};
pub use hub::{Hub, HubConfig, RpcHandler};
pub use registry::{
    ReconnectState, RegistryClient, RegistryEvent, RegistryTransport, RegistryTuning,
};
pub use store::{DocsStore, StoreError};
pub use types::{RoomStatsSnapshot, StaticUrl, SyncError, UrlProvider};
