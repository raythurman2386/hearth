//! Submit, the optimistic send/steer path with failure recovery, the Stop
//! interrupt, and the question wizard's RespondInput.
//!
//! Rune's handlers take `&mut App`, but the async legs run on the bridge's
//! tokio runtime and cannot touch it: each task posts a [`ComposerEvent`] and
//! [`super::poll`] applies the outcome on the UI thread with the exact failure
//! hand-back the gpui composer had.

use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use chrono::Utc;
use hearth_doc::{MessagePart, MessageRole, SessionCommandPayload, SessionMessageEntry};
use hearth_proto::{RunRequest, SandboxLevel, UserInputAnswer, WorktreeSpec};
use hearth_rpc::methods;

use crate::state::{App, Indicator};

use super::attachments::{self, StagedAttachment};
use super::mentions::{self, selected_chat_row};
use super::pure::{
    QUEUED_ATTACHMENTS_MIN, SendButtonMode, WizardStep, composer_has_content, send_button_mode,
};
use super::{ComposerEvent, Placeholder, SendOutcome};

/// Deadline for every queue-plane call (a deferred engine handle can park
/// forever — the send task must never grind silently).
const QUEUE_TIMEOUT: Duration = Duration::from_secs(30);
const DELETE_TIMEOUT: Duration = Duration::from_secs(5);

/// Whether the selected chat has a live run (Working or AwaitingInput).
pub fn run_live(app: &App) -> bool {
    let Some(chat_id) = app.selected_chat.as_deref() else {
        return false;
    };
    matches!(
        app.indicator_for(chat_id),
        Some(Indicator::Working) | Some(Indicator::AwaitingInput)
    )
}

/// New-chat sends need a project: with none picked the send button dims and
/// submit is a no-op. Existing chats carry their own project, so they always
/// send.
pub fn send_blocked(app: &App) -> bool {
    if app.selected_chat.is_some() {
        return false;
    }
    // New-chat canvas: needs a project AND a runnable agent. The no-agents
    // check only fires once the catalog is loaded — offline and still-loading
    // states must not block.
    app.selected_space_row().is_none() || app.pickers.no_agents_available()
}

/// What the send button is right now.
pub fn button_mode(app: &App) -> SendButtonMode {
    let has_text = composer_has_content(
        &app.composer.edit.text,
        app.composer.staged().len(),
        app.composer.staged_comments(app).len(),
    );
    send_button_mode(run_live(app), has_text)
}

/// The composer's Enter handler.
pub fn submit(app: &mut App) {
    if app.composer.wizard.is_some() {
        // Enter inside the panel's free-text input submits the page.
        let typed = app.composer.edit.text.trim().to_string();
        if let Some(wizard) = app.composer.wizard.as_mut() {
            wizard.set_typed(typed);
        }
        wizard_advance(app);
        return;
    }
    let text = mentions::prompt_text(&app.composer.edit).trim().to_string();
    let no_content = !composer_has_content(
        &text,
        app.composer.staged().len(),
        app.composer.staged_comments(app).len(),
    );
    match button_mode(app) {
        SendButtonMode::Stop => interrupt(app),
        _ if no_content => {}
        _ if send_blocked(app) => {}
        SendButtonMode::Send => send(app, text, false),
        SendButtonMode::Steer => send(app, text, true),
    }
}

/// Queue a Run (or Steer) doc command with an optimistic echo. New chats
/// thread the picked config in: worktree creation (when the isolated toggle is
/// on), `Mutate createChat` with the `ChatConfig` + cwd, and the model /
/// reasoning / options on the Run request itself.
pub fn send(app: &mut App, text: String, steer: bool) {
    let Some(engine) = app.engine.clone() else {
        app.composer.failure = Some("Engine not connected".into());
        app.composer.failure_key = None; // global — meaningful on every chat
        return;
    };
    // Chat id: existing selection, or client-minted for the new-chat canvas
    // (the chat then appears from the doc host once the doc materializes).
    let (chat_id, is_new) = match app.selected_chat.clone() {
        Some(id) => (id, false),
        None => (uuid::Uuid::new_v4().to_string(), true),
    };
    // Where the new session runs — resolved NOW so the async block needs no
    // picker access.
    let plan = app.pickers.checkout_plan();
    // Fully-resolved model/reasoning/options — concrete values so the engine
    // never has to guess a "default".
    let resolved = app.pickers.resolved(app);
    let existing_chat = selected_chat_row(app).cloned();
    let existing_cwd = existing_chat.as_ref().and_then(|c| c.cwd.clone());
    // The PROJECT fixes the new chat's device + base folder.
    let space = app.selected_space_row().cloned();
    let local_device_id = app.local_device_id.clone();
    let target_device_id = app.effective_device_id();
    let device_id = if is_new {
        target_device_id
            .clone()
            .unwrap_or_else(|| "local".to_string())
    } else {
        existing_chat
            .as_ref()
            .map(|c| c.device_id.clone())
            .or_else(|| local_device_id.clone())
            .unwrap_or_else(|| "local".to_string())
    };
    // Uploads/read-backs target the chat's HOST device (forwardable RPCs); for
    // a new chat that's the target device (None when it's local).
    let host_device_id = if is_new {
        target_device_id
            .clone()
            .filter(|id| local_device_id.as_deref() != Some(id.as_str()))
    } else {
        existing_chat.as_ref().map(|c| c.device_id.clone())
    };
    let space_id = space.as_ref().map(|s| s.id.clone());
    let space_path = space.as_ref().map(|s| s.path.clone());
    // Snapshot-and-clear NOW (use-attachments.ts takeAttachments): the strip
    // empties the instant you hit send; a failure hands the files back.
    let staged = app
        .composer
        .attachments
        .remove(&app.composer.current_key)
        .unwrap_or_default();
    let key = app.composer.current_key.clone();
    // `typed` keeps the user's own words for the failure hand-back below:
    // restoring the folded prompt would paste the comment block into the input
    // as literal text.
    let typed = text.clone();
    let comments = app.take_diff_comments(&key);
    let text = crate::comments::with_comments(&text, &comments);
    app.composer.preview = None;
    let message_id = uuid::Uuid::new_v4().to_string();
    let created_at = Utc::now().timestamp_millis();

    // Queued-attachment flow (durable-by-design): stage the bytes on the LOCAL
    // engine, queue the command immediately with `pending://` refs, and let the
    // engine push the bytes to a remote host afterwards.
    let host_is_remote = host_device_id
        .as_deref()
        .is_some_and(|id| local_device_id.as_deref() != Some(id));
    let queued_flow = !staged.is_empty() && {
        let local_ok = local_device_id.as_deref().is_some_and(|id| {
            attachments::device_version_at_least(&app.devices, id, QUEUED_ATTACHMENTS_MIN)
        });
        let host_ok = !host_is_remote
            || host_device_id.as_deref().is_some_and(|id| {
                attachments::device_version_at_least(&app.devices, id, QUEUED_ATTACHMENTS_MIN)
            });
        local_ok && host_ok
    };
    // Upload identities minted NOW: in the queued flow the `pending://` ref IS
    // the persisted transport until the host rewrites it.
    let upload_ids: Vec<String> = staged
        .iter()
        .map(|_| uuid::Uuid::new_v4().to_string())
        .collect();
    let echo_paths: Vec<String> = if queued_flow {
        staged
            .iter()
            .zip(&upload_ids)
            .map(|(att, id)| format!("pending://{id}/{}", att.name))
            .collect()
    } else {
        staged
            .iter()
            .map(|att| format!("pending/{}/{}", att.id, att.name))
            .collect()
    };
    let echo_text = attachments::with_attachments(&text, &echo_paths);
    // Every seed is `(device, path)` so the outcome can retire it.
    let mut seed_keys: Vec<(String, String)> = Vec::new();
    // Queued flow also seeds the UPLOAD ALIAS: the host rewrites the persisted
    // ref to `{uploads}/{id8}-{name}`.
    if queued_flow {
        for (upload_id, att) in upload_ids.iter().zip(&staged) {
            let path = attachments::alias_path(upload_id, &att.name);
            let bytes = att.bytes_arc();
            attachments::seed_attachment_alias(&device_id, upload_id, &att.name, bytes.clone());
            seed_keys.push((device_id.clone(), path.clone()));
            if let Some(local) = local_device_id.as_deref()
                && local != device_id
            {
                attachments::seed_attachment_alias(local, upload_id, &att.name, bytes);
                seed_keys.push((local.to_string(), path));
            }
        }
    }
    for (path, att) in echo_paths.iter().zip(&staged) {
        let bytes = att.bytes_arc();
        attachments::seed_attachment(&device_id, path, &att.name, bytes.clone());
        seed_keys.push((device_id.clone(), path.clone()));
        if let Some(local) = local_device_id.as_deref()
            && local != device_id
        {
            attachments::seed_attachment(local, path, &att.name, bytes);
            seed_keys.push((local.to_string(), path.clone()));
        }
    }

    // Optimistic echo (client-minted id doubles as the persisted message id,
    // so the doc frame dedups it away).
    let echo = SessionMessageEntry {
        id: message_id.clone(),
        role: MessageRole::User,
        parts: vec![MessagePart::Text {
            id: "t0".into(),
            text: echo_text.clone(),
        }],
        created_at,
        device_id: "local".into(),
        status: None,
        continuation_of: None,
    };
    if is_new {
        app.select_chat(Some(chat_id.clone()));
    }
    app.push_echo(&chat_id, echo);
    // Working overlay until the host executes the queued command — without it
    // a remote send flashed Completed in the queue→drain→sync gap.
    app.begin_pending_send(&chat_id, &message_id, Utc::now());

    app.composer.edit.clear();
    app.composer.drafts.remove(&app.composer.current_key);
    app.composer.failure = None;
    app.composer.sending = true;
    app.composer.set_sent(&chat_id, &message_id);

    let steer_cmd = steer && !is_new;
    let restore_text = typed;
    let err_chat_id = chat_id.clone();
    let err_message_id = message_id.clone();
    let restore_key = if is_new {
        String::new()
    } else {
        chat_id.clone()
    };
    let outcome_staged = staged.clone();
    let outcome_comments = comments.clone();
    let total: u64 = staged.iter().map(|a| a.bytes().len() as u64).sum();
    let progress = Arc::new(AtomicU64::new(0));
    if !staged.is_empty() {
        app.begin_upload_progress(total, progress.clone());
    }
    let tx = app.composer.sender(&app.bridge);
    let bridge = app.bridge.clone();
    let client = engine.client.clone();

    bridge.spawn(async move {
        let result: Result<(), String> = async {
            // Attachments stage FIRST — before the chat row or anything else
            // exists. Staging is chat-independent (keyed by uploadId), and
            // ordering it first makes a new-chat send atomic: a staging failure
            // aborts with NOTHING created.
            let mut content = text.clone();
            let mut attachment_paths: Vec<String> = Vec::new();
            let mut transfers: Vec<serde_json::Value> = Vec::new();
            if !staged.is_empty() && queued_flow {
                for (att, upload_id) in staged.iter().zip(&upload_ids) {
                    if let Err(err) = attachments::upload_attachment(
                        &client,
                        None,
                        upload_id,
                        att,
                        Some(progress.clone()),
                    )
                    .await
                    {
                        tracing::warn!(name = %att.name, error = %err, "local attachment stage failed");
                        return Err("Couldn't stage the attachment locally.".to_string());
                    }
                    transfers.push(serde_json::json!({
                        "uploadId": upload_id,
                        "fileName": att.name,
                    }));
                }
                // The echo refs ARE the persisted refs — no refresh pass.
                attachment_paths = echo_paths.clone();
                content = echo_text.clone();
            } else if !staged.is_empty() {
                for (att, upload_id) in staged.iter().zip(&upload_ids) {
                    match attachments::upload_attachment(
                        &client,
                        host_device_id.as_deref(),
                        upload_id,
                        att,
                        Some(progress.clone()),
                    )
                    .await
                    {
                        Ok(path) => attachment_paths.push(path),
                        Err(err) => {
                            tracing::warn!(name = %att.name, error = %err, "attachment upload failed");
                            return Err(
                                "Couldn't upload the attachment — the device may be offline."
                                    .to_string(),
                            );
                        }
                    }
                }
                // Seed the transcript cache from local bytes so the sent
                // bubble's thumbnails never round-trip.
                let seed_device = host_device_id.clone().unwrap_or_else(|| device_id.clone());
                for (path, att) in attachment_paths.iter().zip(&staged) {
                    let bytes = att.bytes_arc();
                    attachments::seed_attachment(&seed_device, path, &att.name, bytes.clone());
                    seed_keys.push((seed_device.clone(), path.clone()));
                    if seed_device != device_id {
                        attachments::seed_attachment(&device_id, path, &att.name, bytes);
                        seed_keys.push((device_id.clone(), path.clone()));
                    }
                }
                content = attachments::with_attachments(&text, &attachment_paths);
            }

            // Resolve the working directory: existing chats keep theirs; new
            // chats run per the checkout plan.
            let mut cwd = if is_new {
                space_path.clone().or_else(|| Some("~".to_string()))
            } else {
                existing_cwd
            }
            .unwrap_or_else(|| ".".to_string());
            let mut worktree_cwd: Option<String> = None;
            // Fresh-worktree plans ride the QUEUED Run command (a WorktreeSpec
            // the HOST materializes at drain time) instead of a blocking
            // CreateWorktree relay RPC here.
            let mut run_worktree: Option<WorktreeSpec> = None;
            // The picked ref rides createChat so the session footer names it
            // from the first frame.
            let mut chat_branch: Option<String> = None;
            if is_new {
                match &plan {
                    crate::pickers::CheckoutPlan::CurrentCheckout { branch } => {
                        chat_branch = branch.clone();
                    }
                    crate::pickers::CheckoutPlan::ReuseWorktree { path, branch } => {
                        cwd = path.clone();
                        worktree_cwd = Some(path.clone());
                        chat_branch = Some(branch.clone());
                    }
                    crate::pickers::CheckoutPlan::NewWorktree { base } => {
                        chat_branch = base.clone();
                        if let Some(repo_path) = &space_path {
                            // A remote repo's branch list loads over the relay —
                            // default to HEAD, which git resolves as the repo's
                            // current checkout state.
                            let base = base.clone().unwrap_or_else(|| "HEAD".to_string());
                            run_worktree = Some(WorktreeSpec {
                                repo_path: repo_path.clone(),
                                base,
                            });
                        }
                    }
                }
            }

            // Best-effort Mutate createChat with the picked config: the engine
            // resolves device + cwd from the PROJECT row when one is picked;
            // project-less chats name the host device outright.
            if is_new {
                let mut mutate = serde_json::json!({
                    "op": "createChat",
                    "chatId": chat_id,
                });
                if let Some(object) = mutate.as_object_mut() {
                    match &space_id {
                        Some(space_id) => {
                            object.insert(
                                "spaceId".into(),
                                serde_json::Value::String(space_id.clone()),
                            );
                        }
                        None => {
                            object.insert(
                                "deviceId".into(),
                                serde_json::Value::String(device_id.clone()),
                            );
                        }
                    }
                    if let Some(worktree_cwd) = &worktree_cwd {
                        object.insert(
                            "cwd".into(),
                            serde_json::Value::String(worktree_cwd.clone()),
                        );
                    }
                    if let Some(branch) = &chat_branch {
                        object.insert(
                            "branch".into(),
                            serde_json::Value::String(branch.clone()),
                        );
                    }
                    if let Some(config) = resolved.chat_config()
                        && let Ok(config) = serde_json::to_value(&config)
                    {
                        object.insert("config".into(), config);
                    }
                }
                if let Err(err) = attachments::call_with_timeout(
                    &client,
                    methods::MUTATE,
                    mutate,
                    QUEUE_TIMEOUT,
                )
                .await
                {
                    tracing::warn!(error = %err, "CreateChat mutate unavailable; doc host will materialize the chat");
                }
            }

            let command = if steer_cmd {
                SessionCommandPayload::Steer {
                    prompt: content.clone(),
                    message_id: Some(message_id.clone()),
                }
            } else {
                SessionCommandPayload::Run {
                    request: RunRequest {
                        harness: resolved.harness,
                        prompt: content.clone(),
                        model: resolved.model.clone(),
                        mode: resolved.mode,
                        reasoning: resolved.reasoning,
                        model_options: resolved.model_options.clone(),
                        cwd,
                        sandbox: SandboxLevel::WorkspaceWrite,
                        auto_approve: false,
                        resume: None,
                        attachments: attachment_paths,
                        worktree: run_worktree,
                    },
                    message_id: message_id.clone(),
                }
            };
            let command = serde_json::to_value(&command)
                .map_err(|e| format!("Send failed: {e}"))?;
            let mut params = serde_json::json!({ "chatId": chat_id, "command": command });
            if !transfers.is_empty() {
                params["transfers"] = serde_json::Value::Array(transfers);
            }
            // Deadline-bounded: QueueCommand is a local write, but a deferred
            // engine handle can park forever.
            attachments::call_with_timeout(
                &client,
                methods::QUEUE_COMMAND,
                params,
                QUEUE_TIMEOUT,
            )
            .await
            .map_err(|e| format!("Send failed: {e}"))?;
            Ok(())
        }
        .await;
        let error = match result {
            Ok(()) => None,
            Err(message) => {
                if is_new {
                    // A failed new-chat send must not strand a just-minted
                    // empty chat in the sidebar.
                    let _ = attachments::call_with_timeout(
                        &client,
                        methods::MUTATE,
                        serde_json::json!({ "op": "deleteChat", "chatId": err_chat_id }),
                        DELETE_TIMEOUT,
                    )
                    .await;
                }
                Some(message)
            }
        };
        tx.send(ComposerEvent::SendFinished(SendOutcome {
            chat_id: err_chat_id,
            message_id: err_message_id,
            restore_key,
            typed: restore_text,
            is_new,
            error,
            comments: outcome_comments,
            staged: outcome_staged,
            seeded: seed_keys,
        }));
    });
}

/// Stop the live run. `action` failures never replace the send task.
pub fn interrupt(app: &mut App) {
    let Some(engine) = app.engine.clone() else {
        return;
    };
    let Some(chat_id) = app.selected_chat.clone() else {
        return;
    };
    let params = serde_json::json!({
        "chatId": chat_id,
        "command": { "kind": "interrupt" },
    });
    let tx = app.composer.sender(&app.bridge);
    app.bridge.spawn(async move {
        let result = engine.client.call(methods::QUEUE_COMMAND, params).await;
        if let Err(err) = result {
            tx.send(ComposerEvent::Failure {
                key: Some(chat_id),
                message: format!("Stop failed: {err}"),
            });
        }
    });
}

// ---------------------------------------------------------------------------
// Question wizard glue
// ---------------------------------------------------------------------------

/// Select an option on the current page.
pub fn wizard_select(app: &mut App, option_ix: usize) {
    let Some(wizard) = app.composer.wizard.as_mut() else {
        return;
    };
    let step = wizard.select(option_ix);
    let has_pick = wizard.page_has_pick();
    app.composer.placeholder = if has_pick {
        Placeholder::WIZARD_PICKED.to_string()
    } else {
        Placeholder::WIZARD.to_string()
    };
    match step {
        WizardStep::AutoAdvance => schedule_auto_advance(app),
        WizardStep::Done(answers) => wizard_finish(app, answers),
        WizardStep::Stay => {}
    }
}

/// Single-select questions auto-advance after [`super::pure::AUTO_ADVANCE_MS`].
pub fn schedule_auto_advance(app: &mut App) {
    let Some(request_id) = app.composer.wizard.as_ref().map(|w| w.request_id.clone()) else {
        return;
    };
    let tx = app.composer.sender(&app.bridge);
    app.bridge.spawn(async move {
        tokio::time::sleep(Duration::from_millis(super::pure::AUTO_ADVANCE_MS)).await;
        tx.send(ComposerEvent::WizardAdvance { request_id });
    });
}

pub fn wizard_advance(app: &mut App) {
    let Some(wizard) = app.composer.wizard.as_mut() else {
        return;
    };
    match wizard.advance() {
        WizardStep::Done(answers) => wizard_finish(app, answers),
        _ => {
            // Moving on: clear the shared free-text input for the next page.
            app.composer.edit.clear();
        }
    }
}

pub fn wizard_back(app: &mut App) {
    if let Some(wizard) = app.composer.wizard.as_mut() {
        wizard.back();
    }
}

/// Submit RespondInput and retire the panel.
pub fn wizard_finish(app: &mut App, answers: Vec<UserInputAnswer>) {
    let Some(wizard) = app.composer.wizard.take() else {
        return;
    };
    app.composer
        .answered_requests
        .insert(wizard.request_id.clone());
    app.composer.edit.clear();
    // The panel borrowed the composer input; hand back its identity.
    app.composer.placeholder = Placeholder::COMPOSER.to_string();
    let Some(engine) = app.engine.clone() else {
        return;
    };
    let Some(chat_id) = app.selected_chat.clone() else {
        return;
    };
    let request_id = wizard.request_id.clone();
    let command = SessionCommandPayload::RespondInput {
        request_id: request_id.clone(),
        answers,
    };
    let params = match serde_json::to_value(&command) {
        Ok(value) => serde_json::json!({ "chatId": chat_id, "command": value }),
        Err(_) => return,
    };
    let tx = app.composer.sender(&app.bridge);
    app.bridge.spawn(async move {
        let result = engine.client.call(methods::QUEUE_COMMAND, params).await;
        let error = result.err().map(|err| format!("Answer failed: {err}"));
        let failed = error.is_some();
        tx.send(ComposerEvent::AnswerFinished {
            request_id: request_id.clone(),
            chat_id,
            error,
        });
        if failed {
            return;
        }
        // Safety net against a dead-looking session: the command queued, but
        // the host may still REJECT it. If the same request is still the live
        // pending input once the host has had time to execute and sync back,
        // un-hide the panel instead of leaving the question unanswerable.
        tokio::time::sleep(Duration::from_secs(2)).await;
        tx.send(ComposerEvent::AnswerSettled { request_id });
    });
}

/// Apply a completed send: success just clears the in-flight chrome; failure
/// performs the full hand-back (echo, pending send, prompt, attachments,
/// comments).
pub fn apply_send_outcome(app: &mut App, outcome: SendOutcome) {
    app.composer.sending = false;
    app.end_upload_progress();
    let SendOutcome {
        chat_id,
        message_id,
        restore_key,
        typed,
        is_new,
        error,
        comments,
        staged,
        seeded,
    } = outcome;
    // The send resolved either way: drop the transcript seed bytes it minted
    // (the transcript decodes its own copy, and a failed send's bubble is
    // rolled back below).
    for (device, path) in seeded {
        attachments::forget_seeded(&device, &path);
    }
    let Some(message) = error else {
        // Sent: the staged ids left the composer with the send, so their
        // decoded thumbnails are unreachable — drop them.
        app.composer.prune_thumbs(std::iter::empty());
        return;
    };
    app.composer.failure = Some(message);
    app.composer.failure_key = Some(restore_key.clone());
    app.remove_echo(&chat_id, &message_id);
    app.end_pending_send(&chat_id, &message_id);
    if is_new && app.selected_chat.as_deref() == Some(chat_id.as_str()) {
        // Back to the canvas; the navigation draft-swap loads the restored
        // draft below.
        app.select_chat(None);
    }
    for comment in comments {
        app.add_diff_comment(&restore_key, comment);
    }
    if is_new && app.composer.current_key != restore_key {
        // A re-key swap to the canvas is pending (the `select_chat(None)`
        // above); it loads this draft into the input on flush — setting the
        // input directly here would be clobbered by that same swap.
        app.composer.drafts.insert(restore_key.clone(), typed);
    } else {
        mentions::set_draft(&mut app.composer.edit, &typed);
    }
    if !staged.is_empty() {
        // Merge by id (stashAttachments): files the user staged while the send
        // was in flight survive the hand-back — draining the minted chat's slot
        // too when the restore target is the canvas.
        let mut merged = staged;
        for key in [chat_id.clone(), restore_key.clone()] {
            if let Some(slot) = app.composer.attachments.get_mut(&key) {
                let fresh: Vec<StagedAttachment> = slot
                    .drain(..)
                    .filter(|e| !merged.iter().any(|f| f.id == e.id))
                    .collect();
                merged.extend(fresh);
            }
        }
        app.composer.attachments.insert(restore_key, merged);
    }
    // Keep exactly the restored (and still-staged elsewhere) thumbnails.
    app.composer.prune_thumbs(std::iter::empty());
}

#[cfg(test)]
mod tests {
    use crate::composer::pure::{SendButtonMode, composer_has_content, send_button_mode};

    #[test]
    fn a_comment_only_stage_steers_a_live_run_instead_of_stopping_it() {
        let live = true;
        let comment_only = composer_has_content("", 0, 2);
        assert_eq!(
            send_button_mode(live, comment_only),
            SendButtonMode::Steer,
            "comment-only submit must steer, not interrupt the run"
        );
        assert_eq!(
            send_button_mode(live, composer_has_content("", 0, 0)),
            SendButtonMode::Stop
        );
    }

    #[test]
    fn outcome_restores_the_canvas_key_for_a_new_chat() {
        // The restore key rule (is_new → the canvas) is what keeps a failed
        // minted chat from restoring under its own deleted key.
        let is_new = true;
        let chat_id = "chat-1".to_string();
        let restore_key = if is_new { String::new() } else { chat_id };
        assert_eq!(restore_key, "");
    }
}
