//! Per-domain `EngineRpc` dispatch handlers. [`super::EngineRpc::handle`] keeps
//! the single method→domain table; each module owns its methods' arms and the
//! param structs they parse.

pub(super) mod agents;
pub(super) mod chat;
pub(super) mod diffs;
pub(super) mod repos;
pub(super) mod terminals;
pub(super) mod uploads;
pub(super) mod workspace;
