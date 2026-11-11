//! The async bridge between the engine's tokio world and Rune's single-threaded
//! UI.
//!
//! Rune has no `gpui_tokio`: background work runs on a private tokio runtime and
//! posts [`EngineEvent`]s over an `std::sync::mpsc` channel. `AppSpec::poll`
//! drains the channel once per frame; the [`Waker`] Rune hands to `setup` is
//! called by the producer so a frame is scheduled even while `poll` would
//! otherwise return false.
//!
//! Watch pumps are tasks on the runtime that resubscribe on stream end. Each
//! returns a [`WatchGuard`]; aborting the guard cancels the engine-side watch
//! (the RPC client's drop sends the cancel frame).

use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};

use rune::runtime::Waker;

use hearth_doc::TranscriptFrame;
use hearth_proto::{
    AuthState, Chat, Connectivity, Device, Session, Space, TransferProgress,
};
use hearth_rpc::RpcClient;
use hearth_update::UpdateStatus;

use crate::engine::EngineHandle;

/// Delay before resubscribing a watch whose stream ended.
pub const WATCH_RETRY: std::time::Duration = std::time::Duration::from_secs(2);

/// One frame of engine state, delivered to the UI thread.
pub enum EngineEvent {
    /// Bootstrap won: a connected (or embedded) engine.
    Ready(Box<EngineHandle>),
    /// Bootstrap failed; the message is user-facing.
    Failed(String),
    Sessions(Vec<Session>),
    Chats(Vec<Chat>),
    Devices(Vec<Device>),
    Spaces(Vec<Space>),
    Connectivity(Connectivity),
    Auth(AuthState),
    Update(UpdateStatus),
    Transfers(Vec<TransferProgress>),
    /// A `WatchDocMessages` delta frame for the selected chat.
    TranscriptFrame {
        chat_id: String,
        frame: TranscriptFrame,
    },
    /// A `WatchDocMessages` delta frame for a subagent doc.
    SubagentFrame {
        doc_id: String,
        frame: TranscriptFrame,
    },
}

/// Handle to a running watch pump. Cancelling aborts the task.
#[derive(Debug)]
pub struct WatchGuard {
    handle: tokio::task::JoinHandle<()>,
}

impl WatchGuard {
    pub fn cancel(&self) {
        self.handle.abort();
    }
}

/// Dropping a guard aborts its pump: replacing `App::transcript_guard` must
/// cancel the old watch, not leave it resubscribing forever.
impl Drop for WatchGuard {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

struct BridgeInner {
    runtime: tokio::runtime::Runtime,
    event_tx: Sender<EngineEvent>,
    event_rx: Mutex<Receiver<EngineEvent>>,
    waker: Mutex<Option<Waker>>,
}

/// The tokio runtime, the event channel and the Rune waker. Cheap to clone;
/// background tasks hold a clone and post events back to the UI thread.
#[derive(Clone)]
pub struct Bridge {
    inner: Arc<BridgeInner>,
}

impl Bridge {
    pub fn new() -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("failed to start the engine tokio runtime");
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        Self {
            inner: Arc::new(BridgeInner {
                runtime,
                event_tx,
                event_rx: Mutex::new(event_rx),
                waker: Mutex::new(None),
            }),
        }
    }

    /// Store the wake handle Rune gave us in `setup`.
    pub fn set_waker(&self, waker: Waker) {
        *self.inner.waker.lock().unwrap() = Some(waker);
    }

    /// Wake the Rune loop (schedules a `poll`).
    pub fn wake(&self) {
        if let Some(waker) = self.inner.waker.lock().unwrap().as_ref() {
            waker();
        }
    }

    /// Queue an event and wake the UI.
    pub fn send(&self, event: EngineEvent) {
        let _ = self.inner.event_tx.send(event);
        self.wake();
    }

    /// Drain one queued event; `Empty` when the channel is drained.
    pub fn try_recv(&self) -> Result<EngineEvent, TryRecvError> {
        self.inner.event_rx.lock().unwrap().try_recv()
    }

    pub fn runtime(&self) -> &tokio::runtime::Runtime {
        &self.inner.runtime
    }

    /// Spawn a resubscribing watch pump. `map` turns a raw stream frame into an
    /// [`EngineEvent`]; `None` drops a malformed frame.
    pub fn spawn_watch<F>(
        &self,
        client: Arc<RpcClient>,
        method: &'static str,
        params: serde_json::Value,
        map: F,
    ) -> WatchGuard
    where
        F: Fn(serde_json::Value) -> Option<EngineEvent> + Send + 'static,
    {
        let tx = self.inner.event_tx.clone();
        let waker = self.inner.waker.lock().unwrap().clone();
        let handle = self.inner.runtime.spawn(async move {
            loop {
                let mut rx = match client.subscribe(method, params.clone()).await {
                    Ok(rx) => rx,
                    Err(err) => {
                        tracing::debug!(%method, error = %err, "watch unavailable; retrying");
                        tokio::time::sleep(WATCH_RETRY).await;
                        continue;
                    }
                };
                while let Some(value) = rx.recv().await {
                    if let Some(event) = map(value) {
                        let _ = tx.send(event);
                        if let Some(waker) = &waker {
                            waker();
                        }
                    }
                }
                tracing::debug!(%method, "watch stream ended; resubscribing");
                tokio::time::sleep(WATCH_RETRY).await;
            }
        });
        WatchGuard { handle }
    }

    /// Spawn a one-shot async job (an RPC call, an upload, a mutate).
    pub fn spawn<F>(&self, job: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        self.inner.runtime.spawn(job);
    }
}
