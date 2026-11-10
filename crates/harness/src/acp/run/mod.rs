//! The live session loop: one task multiplexing agent messages, the pending
//! turn, the steering mailbox, the interrupt token, and consumer liveness.

use std::collections::VecDeque;
use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use hearth_proto::{AgentEvent, DoneStatus, SlashCommand};

use crate::jsonrpc::Incoming;
use crate::{HarnessError, RunControls, Signal, send_signal, shutdown_child};

use super::config::{config_option_sets, first_class_model_change};
use super::discovery::scan_available_commands;
use super::new_message_id;
use super::session::{Session, initialize_params, steering_supported};
use super::subagent::SubagentTracker;
use super::turn::{
    SubagentObserver, handle_server_request_live, prompt_turn, request_draining,
    session_update_events, steering_call_future, stop_outcome, track_turn_signals,
    usage_from_response,
};

/// The per-run event loop: one task multiplexing agent messages, the pending
/// turn, the steering mailbox, the interrupt token, and consumer liveness.
/// Rotate the assistant message id; returns (previous, next).
fn rotate(id: &mut String) -> (String, String) {
    let prev = std::mem::replace(id, new_message_id());
    (prev, id.clone())
}

async fn send(tx: &mpsc::Sender<Result<AgentEvent, HarnessError>>, ev: AgentEvent) -> bool {
    tx.send(Ok(ev)).await.is_ok()
}

pub(crate) async fn run_session(session: Session) {
    let Session {
        mut child,
        client,
        mut incoming,
        event_tx,
        controls,
        request,
        harness,
        agent_name,
        prompt_complete_extension,
        prompt_stall,
        stall_hint,
        sessions_root,
        prompt_transform,
        effort_values,
        interrupt_grace,
        kill_grace,
        handshake_timeout,
        stderr_tail,
    } = session;
    let RunControls {
        request_input,
        mut steering,
        interrupt,
    } = controls;
    let request_input = std::sync::Arc::new(request_input);

    // ---- handshake + session (interruptible) ------------------------------
    let setup = async {
        let init = client
            .request("initialize", initialize_params(harness))
            .await?;
        let steer_ext = steering_supported(&init);
        let init_commands = scan_available_commands(&init);

        let session_params = json!({ "cwd": request.cwd, "mcpServers": [] });
        let (session_id, session_response) = if let Some(resume) = &request.resume {
            let mut load = session_params.clone();
            load["sessionId"] = Value::String(resume.clone());
            match request_draining(&client, &mut incoming, "session/load", load).await {
                Ok(resp) => (resume.clone(), resp),
                // A missing/foreign session falls back to a fresh one.
                Err(e) => {
                    tracing::debug!(
                        target: "hearth_harness::acp",
                        "session/load failed (starting fresh): {e}"
                    );
                    let new = request_draining(
                        &client,
                        &mut incoming,
                        "session/new",
                        session_params.clone(),
                    )
                    .await?;
                    (
                        new.get("sessionId")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        new,
                    )
                }
            }
        } else {
            let new =
                request_draining(&client, &mut incoming, "session/new", session_params).await?;
            (
                new.get("sessionId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                new,
            )
        };
        if session_id.is_empty() {
            return Err(HarnessError::Protocol(
                "session/new returned no sessionId".into(),
            ));
        }
        // Apply the run's requested interaction mode (plan/agent/chat) via
        // `session/set_mode`. Best-effort: agents that don't advertise modes
        // (or reject the id) keep their own default — a rejected mode switch
        // is logged, not fatal, mirroring the auxiliary config options below.
        if let Some(mode) = request.mode
            && let Err(e) = request_draining(
                &client,
                &mut incoming,
                "session/set_mode",
                json!({
                    "sessionId": session_id,
                    "modeId": mode.as_str(),
                }),
            )
            .await
        {
            tracing::debug!(
                target: "hearth_harness::acp",
                "session/set_mode {} rejected (agent default runs): {e}",
                mode.as_str()
            );
        }
        // ACP has had two model-selection surfaces. Newer config-option agents
        // use category=model below; Grok Build currently advertises only the
        // first-class `models` state and requires `session/set_model`. Paseo
        // follows the same split. Unlike the best-effort auxiliary options,
        // an explicit model switch is strict: prompting with a different
        // model than the picker shows is worse than surfacing the RPC error.
        if let Some(model) = first_class_model_change(&session_response, request.model.as_deref())?
        {
            request_draining(
                &client,
                &mut incoming,
                "session/set_model",
                json!({
                    "sessionId": session_id,
                    "modelId": model,
                }),
            )
            .await
            .map_err(|error| {
                HarnessError::Protocol(format!("agent rejected model switch to {model}: {error}"))
            })?;
        }
        // Apply the run's model + effort + model options through the
        // session's advertised config options. Best-effort for effort and
        // traits: a rejected auxiliary set is logged and the agent default
        // runs.
        let efforts = effort_values(request.reasoning, request.model.as_deref());
        let requested_model = request.model.as_deref();
        let options_snapshot = session_response;
        for (config_id, payload) in config_option_sets(
            &options_snapshot,
            requested_model,
            &efforts,
            &request.model_options,
            request.mode,
        ) {
            let mut params = serde_json::Map::new();
            params.insert("sessionId".into(), session_id.clone().into());
            params.insert("configId".into(), config_id.clone().into());
            if let Some(payload) = payload.as_object() {
                for (k, v) in payload {
                    params.insert(k.clone(), v.clone());
                }
            }
            if let Err(e) = request_draining(
                &client,
                &mut incoming,
                "session/set_config_option",
                Value::Object(params),
            )
            .await
            {
                tracing::debug!(
                    target: "hearth_harness::acp",
                    "session/set_config_option {config_id}={payload} rejected (agent default runs): {e}"
                );
            }
        }
        Ok::<(String, bool, Vec<SlashCommand>), HarnessError>((
            session_id,
            steer_ext,
            init_commands,
        ))
    };
    let (session_id, steer_ext, init_commands) = tokio::select! {
        res = tokio::time::timeout(handshake_timeout, setup) => {
            let res = res.unwrap_or_else(|_| {
                // A hung handshake (agent waiting on a login it can never
                // get, a wedged adapter) used to spin "Working" forever —
                // the false "thinking for 2+ minutes then nothing" class of
                // report. Bound it and say what was reached.
                Err(HarnessError::Protocol(format!(
                    "{agent_name} did not complete the ACP handshake within {}s \
                     (the agent may be waiting for a login — try running it once \
                     in a terminal)",
                    handshake_timeout.as_secs()
                )))
            });
            match res {
                Ok(v) => v,
                Err(e) => {
                    // A child that dies before the handshake used to surface only
                    // the RPC-side symptom ("transport closed") — its exit status
                    // and stderr, both already in hand, were dropped, leaving
                    // startup crashes undiagnosable (user report). When the child
                    // is already gone, give the reader task a beat to drain the
                    // pipe, then append the crash text; the Done carrying it is
                    // journaled, so the cause survives for later inspection. A
                    // still-live child (the timeout) contributes its stderr tail.
                    let error = match child.try_wait() {
                        Ok(Some(status)) => {
                            tokio::time::sleep(Duration::from_millis(200)).await;
                            format!(
                                "{e}; {}",
                                crate::crash_message(agent_name, Some(status), &stderr_tail)
                            )
                        }
                        _ => match stderr_tail.snapshot() {
                            Some(tail) => format!("{e}; stderr: {tail}"),
                            None => e.to_string(),
                        },
                    };
                    tracing::warn!(target: "hearth_harness::acp", %error, "agent setup failed");
                    let _ = event_tx
                        .send(Ok(AgentEvent::Done {
                            status: DoneStatus::Errored,
                            result: None,
                            error: Some(error),
                            session_id: None,
                        }))
                        .await;
                    shutdown_child(&mut child, kill_grace).await;
                    return;
                }
            }
        },
        _ = interrupt.cancelled() => {
            let _ = event_tx
                .send(Ok(AgentEvent::Done {
                    status: DoneStatus::Interrupted,
                    result: None,
                    error: None,
                    session_id: None,
                }))
                .await;
            shutdown_child(&mut child, kill_grace).await;
            return;
        }
    };

    let mut assistant_message_id = new_message_id();
    if !send(
        &event_tx,
        AgentEvent::SessionStarted {
            harness,
            model: request.model.clone().unwrap_or_default(),
            tools: Vec::new(),
            cwd: request.cwd.clone(),
            session_id: session_id.clone(),
            assistant_message_id: assistant_message_id.clone(),
        },
    )
    .await
    {
        shutdown_child(&mut child, kill_grace).await;
        return;
    }
    if !init_commands.is_empty()
        && !send(
            &event_tx,
            AgentEvent::AvailableCommands {
                commands: init_commands,
            },
        )
        .await
    {
        shutdown_child(&mut child, kill_grace).await;
        return;
    }

    // Subagent correlation + transcript tails: the grok tracker (inert for
    // agents that never emit the `subagent_*` extension updates).
    let mut subagents = SubagentObserver(SubagentTracker::new(
        session_id.clone(),
        event_tx.clone(),
        sessions_root,
    ));

    // ---- main loop --------------------------------------------------------
    // Prompt-completion settlement state (the prompt-complete extension):
    // one prompt is outstanding at a time, identified by `current_prompt_id`;
    // settled ids are remembered so a STALE `prompt_complete` (a late replay
    // of an already-settled prompt) can never settle a newer turn.
    let mut prompt_seq: u64 = 0;
    // Assigned before first use by the initial `turn` construction below.
    let mut current_prompt_id: Option<String>;
    let mut completed_prompts: VecDeque<String> = VecDeque::new();
    // `HEARTH_ACP_PROMPT_STALL_MS` overrides the spec's bound; 0 disables.
    let prompt_stall: Option<Duration> = match std::env::var("HEARTH_ACP_PROMPT_STALL_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        Some(0) => None,
        Some(ms) => Some(Duration::from_millis(ms)),
        None => prompt_stall,
    };
    let mut prompt_stall_deadline: Option<tokio::time::Instant> =
        prompt_stall.map(|d| tokio::time::Instant::now() + d);
    let mut turn: Option<BoxFuture<'static, Result<Value, HarnessError>>> = Some({
        prompt_seq += 1;
        current_prompt_id = prompt_complete_extension.then(|| format!("hearth-p{prompt_seq}"));
        prompt_turn(
            client.clone(),
            session_id.clone(),
            prompt_transform(request.reasoning, &request.prompt),
            current_prompt_id.clone(),
        )
    });
    // Steers waiting for the turn boundary (agents without the extension, or
    // extension steers that lost the turn-end race).
    let mut queued_steers: VecDeque<String> = VecDeque::new();
    // The in-flight `_session/steering` call (text + response future), plus
    // followers awaiting their turn. Polled from the main select so the loop
    // keeps draining `incoming` while the agent responds — awaiting inline
    // deadlocks against a full incoming channel when the agent floods
    // updates (the reader blocks on the channel and never parses the
    // steering response).
    let mut steering_call: Option<(String, BoxFuture<'static, Result<Value, HarnessError>>)> = None;
    let mut steer_backlog: VecDeque<String> = VecDeque::new();
    let mut steering_open = true;
    let mut interrupted = false;
    let mut interrupt_sent = false;
    let mut done_current = false;
    let mut done_after_interrupt = false;
    let mut escalation: Option<tokio::task::JoinHandle<()>> = None;
    // Starved-turn recovery (2026-08-12 stuck-Working incident): a
    // `session/prompt` sent while the agent runs a SELF-CONTINUED turn (a
    // background-task re-invocation no prompt started) starves —
    // claude-agent-acp does not track turns it did not start, so the merged
    // turn's result is never attributed to the pending prompt (reproduced
    // against 0.66.0; the prompt's TEXT still reaches the model, queued by
    // the CLI). The tell is protocol evidence, not timing: a steering call
    // answered `promptRequired`/`noRunningTurn` while OUR prompt is
    // outstanding means the adapter has no turn that could ever settle it.
    // A short grace covers the true turn-end race (its response lands within
    // milliseconds); past it, the dead prompt is closed out with a Done and
    // the queued steer is promoted to a fresh turn.
    const STARVE_GRACE: Duration = Duration::from_secs(2);
    let mut starve_deadline: Option<tokio::time::Instant> = None;
    // BLANKET dropped-reply settle, adapter-agnostic: any ACP agent whose
    // prompt response goes missing must not strand the turn. Signals that
    // exist in core ACP stand in for the adapter-specific cost frame: once
    // the turn has streamed content, every tool call it opened has resolved,
    // no permission/question round-trip is pending, and the stream has been
    // quiet past the window, the turn is settled through the same recovery
    // arm. A false settle is only PARTLY recoverable: the engine folds any
    // later output as a self-continued segment and re-arms Working, but the
    // turn is orphaned — the real response resolves a closed channel, no
    // Done ever comes, and the session strands Working until the engine's
    // quiesce watchdog parks it.
    //
    // `HEARTH_ACP_QUIET_SETTLE_MS` overrides; 0 disables.
    let quiet_settle: Option<Duration> = match std::env::var("HEARTH_ACP_QUIET_SETTLE_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        Some(0) => None,
        Some(ms) => Some(Duration::from_millis(ms)),
        None => Some(Duration::from_secs(30)),
    };
    let mut last_update_at = tokio::time::Instant::now();
    let mut turn_content_seen = false;
    let mut open_tools: std::collections::HashSet<String> = std::collections::HashSet::new();
    // An execute-kind tool this turn (even after it resolves) means silence
    // is often a compile/link with no stdout — the 30s blanket settle was
    // firing mid-`cargo build` on the live hub (2026-08-24), synthesizing
    // Done, then more output resumed as self-continued. `0` disables the
    // extension (tests that pin the generic 30s path).
    let exec_quiet: Option<Duration> = match std::env::var("HEARTH_ACP_EXEC_QUIET_SETTLE_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        Some(0) => None,
        Some(ms) => Some(Duration::from_millis(ms)),
        None => Some(Duration::from_secs(180)),
    };
    let mut had_exec = false;
    let open_questions = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    // PREVENTION, ahead of all the recovery above: never send a
    // `session/prompt` into a session that is visibly mid SELF-CONTINUED
    // turn — that prompt's reply is what the adapter drops (the verified
    // starve). Visibly busy = an open tool call, or stream traffic within
    // BUSY_RECENT, with no prompt of ours outstanding. The discipline is
    // Zed's, verified against the real adapter: `session/cancel` the
    // unowned turn, give it CANCEL_FLUSH to die and drain, then prompt.
    // This makes the interactive path starve-free; the settle layers below
    // remain for the notification race a client cannot see coming.
    const BUSY_RECENT: Duration = Duration::from_secs(3);
    const CANCEL_FLUSH: Duration = Duration::from_secs(2);
    let mut cancel_flush_deadline: Option<tokio::time::Instant> = None;

    'main: loop {
        tokio::select! {
            res = async { turn.as_mut().expect("guarded by if").await }, if turn.is_some() => {
                turn = None;
                starve_deadline = None;
                prompt_stall_deadline = None;
                if let Some(id) = current_prompt_id.take() {
                    completed_prompts.push_back(id);
                    while completed_prompts.len() > 32 {
                        completed_prompts.pop_front();
                    }
                }
                // Settle an in-flight `_session/steering` call BEFORE closing
                // the turn: its response rides the same stdout as the prompt
                // response, so by now it is (nearly always) already parsed —
                // the select just hadn't polled it yet. Deciding it here keeps
                // the ordering deterministic: an injection that landed in this
                // turn emits its Steered boundary now, ahead of the drained
                // tail and the Done (a Steered AFTER Done re-armed the
                // consumer with no next turn — the stranded-Working bug); a
                // rejected/unsettled call redelivers as the next turn. The
                // timeout guards the flooded-incoming edge (reader blocked on
                // a full channel never parses the response): past it the call
                // is abandoned and the steer redelivered.
                if let Some((text, mut fut)) = steering_call.take() {
                    let outcome = match tokio::time::timeout(
                        Duration::from_millis(1000),
                        &mut fut,
                    )
                    .await
                    {
                        Ok(Ok(resp)) => resp
                            .get("outcome")
                            .and_then(Value::as_str)
                            .unwrap_or("injected")
                            .to_owned(),
                        Ok(Err(_)) | Err(_) => "promptRequired".to_owned(),
                    };
                    if interrupted {
                        // Winding down; abandoned like any queued steer.
                    } else if outcome != "promptRequired" {
                        let (prev, next) = rotate(&mut assistant_message_id);
                        if !send(
                            &event_tx,
                            AgentEvent::Steered {
                                assistant_message_id: Some(prev),
                                next_assistant_message_id: Some(next),
                            },
                        )
                        .await
                        {
                            break 'main;
                        }
                    } else {
                        queued_steers.push_back(text);
                    }
                    // Followers waiting on the settled call have no live turn
                    // to inject into anymore: boundary delivery.
                    while let Some(next_text) = steer_backlog.pop_front() {
                        queued_steers.push_back(next_text);
                    }
                }
                // Updates streamed before the prompt response are already
                // queued in stdout order — fold them into the turn before
                // closing it (responses bypass the incoming queue).
                let mut consumer_gone = false;
                while let Ok(inc) = incoming.try_recv() {
                    match inc {
                        Incoming::Notification { method, params } => {
                            let events =
                                session_update_events(&method, &params, &session_id, &mut subagents);
                            for ev in events {
                                if !send(&event_tx, ev).await {
                                    consumer_gone = true;
                                    break;
                                }
                            }
                        }
                        Incoming::Request { id, method, params } => {
                            for ev in handle_server_request_live(
                                &client,
                                id,
                                &method,
                                &params,
                                &request_input,
                                &open_questions,
                            ) {
                                if !send(&event_tx, ev).await {
                                    consumer_gone = true;
                                    break;
                                }
                            }
                        }
                        _ => {}
                    }
                    if consumer_gone {
                        break;
                    }
                }
                if consumer_gone {
                    break 'main;
                }
                let (prev, _next) = rotate(&mut assistant_message_id);
                if !send(
                    &event_tx,
                    AgentEvent::AssistantMessageCompleted { assistant_message_id: prev },
                )
                .await
                {
                    break 'main;
                }
                // Per-turn token usage, when the adapter settles the prompt
                // with it (claude-agent-acp and codex-acp both do).
                if let Some(usage) = usage_from_response(&res)
                    && !send(&event_tx, usage).await
                {
                    break 'main;
                }
                let (status, error) = stop_outcome(&res, interrupted);
                done_current = true;
                if interrupted {
                    done_after_interrupt = true;
                }
                if !send(
                    &event_tx,
                    AgentEvent::Done {
                        status,
                        result: None,
                        error,
                        session_id: Some(session_id.clone()),
                    },
                )
                .await
                {
                    break 'main;
                }
                if interrupted || res.is_err() {
                    break 'main;
                }
                // Persistent session: a queued steer becomes the next turn;
                // otherwise stay alive for the mailbox — the caller owns
                // teardown (mirrors the codex harness).
                if let Some(text) = queued_steers.pop_front() {
                    let (prev, next) = rotate(&mut assistant_message_id);
                    if !send(
                        &event_tx,
                        AgentEvent::Steered {
                            assistant_message_id: Some(prev),
                            next_assistant_message_id: Some(next),
                        },
                    )
                    .await
                    {
                        break 'main;
                    }
                    done_current = false;
                    turn_content_seen = false;
                    open_tools.clear();
                    had_exec = false;
                    last_update_at = tokio::time::Instant::now();
                    prompt_seq += 1;
                    current_prompt_id =
                        prompt_complete_extension.then(|| format!("hearth-p{prompt_seq}"));
                    prompt_stall_deadline =
                        prompt_stall.map(|d| tokio::time::Instant::now() + d);
                    turn = Some(prompt_turn(
                        client.clone(),
                        session_id.clone(),
                        text,
                        current_prompt_id.clone(),
                    ));
                } else if !steering_open {
                    break 'main;
                }
            },

            inc = incoming.recv() => match inc {
                Some(Incoming::Notification { method, params }) => {
                    last_update_at = tokio::time::Instant::now();
                    // Wire traffic is a sign of life for the prompt-stall
                    // watchdog — EXCEPT session boilerplate: opencode emits
                    // available_commands_update right after session/new on
                    // every session, including ones whose provider is down
                    // (where it then retries the provider stream forever
                    // with nothing further on the wire — verified live,
                    // 1.18.18). One such frame must not disarm the watchdog
                    // for the whole turn; only turn progress counts.
                    let boilerplate = method == "session/update"
                        && matches!(
                            params
                                .get("update")
                                .and_then(|u| u.get("sessionUpdate"))
                                .and_then(Value::as_str),
                            Some("available_commands_update")
                                | Some("config_option_update")
                                | Some("current_mode_update")
                        );
                    if !boilerplate {
                        prompt_stall_deadline = None;
                    }
                    // `_x.ai/session/prompt_complete` — the AUTHORITATIVE
                    // turn end for agents advertising it (grok): the
                    // `session/prompt` RPC can hang after the turn really
                    // finished. Settle through the SAME response arm by
                    // swapping the in-flight future for the synthesized
                    // response; the abandoned RPC future drains in the
                    // background and its late result is discarded. Guards:
                    // session match, an outstanding prompt, and an exact
                    // prompt-id match (a stale replay of an already-settled
                    // prompt must never settle a newer turn; grok echoes the
                    // `_meta.promptId` we mint — verified live, 1.0.4).
                    if prompt_complete_extension
                        && method == "_x.ai/session/prompt_complete"
                        && !interrupted
                        && turn.is_some()
                        && params.get("sessionId").and_then(Value::as_str)
                            == Some(session_id.as_str())
                    {
                        let pid = params
                            .get("promptId")
                            .and_then(Value::as_str)
                            .map(str::to_owned);
                        let stale = pid.as_deref().is_some_and(|p| {
                            completed_prompts.iter().any(|c| c == p)
                        }) || (pid.is_some() && pid != current_prompt_id);
                        if !stale {
                            let stop = params
                                .get("stopReason")
                                .and_then(Value::as_str)
                                .unwrap_or("end_turn")
                                .to_owned();
                            if let Some(old) = turn.take() {
                                tokio::spawn(async move {
                                    let _ = old.await;
                                });
                            }
                            turn = Some(Box::pin(async move {
                                Ok(json!({ "stopReason": stop }))
                            }));
                        }
                    }
                    // Other notifications (other sessions, agent noise) are
                    // tolerated by design.
                    let events =
                        session_update_events(&method, &params, &session_id, &mut subagents);
                    for ev in events {
                        track_turn_signals(
                            &ev,
                            &mut turn_content_seen,
                            &mut open_tools,
                            &mut had_exec,
                        );
                        if !send(&event_tx, ev).await {
                            break 'main;
                        }
                    }
                }
                Some(Incoming::Request { id, method, params }) => {
                    prompt_stall_deadline = None;
                    for ev in handle_server_request_live(
                        &client,
                        id,
                        &method,
                        &params,
                        &request_input,
                        &open_questions,
                    ) {
                        if !send(&event_tx, ev).await {
                            break 'main;
                        }
                    }
                }
                Some(Incoming::Eof) | None => {
                    // The turn ends via a request RESPONSE, which races EOF
                    // through a different channel than notifications: an agent
                    // exiting right after its final response must read as a
                    // clean finish, not a crash. The response (if any) is
                    // already resolved by the reader before it sends Eof.
                    // Only a RESOLVED response is a clean finish; a request
                    // failed by the reader's EOF cleanup falls through to the
                    // crash-message bookkeeping below (stderr tail intact).
                    if let Some(mut fut) = turn.take() {
                        match tokio::time::timeout(Duration::from_millis(50), &mut fut).await {
                            Ok(Ok(res)) => {
                                let (prev, _next) = rotate(&mut assistant_message_id);
                                let _ = send(
                                    &event_tx,
                                    AgentEvent::AssistantMessageCompleted { assistant_message_id: prev },
                                )
                                .await;
                                if let Some(usage) = usage_from_response(&Ok(res.clone())) {
                                    let _ = send(&event_tx, usage).await;
                                }
                                let (status, error) = stop_outcome(&Ok(res), interrupted);
                                done_current = true;
                                if interrupted {
                                    done_after_interrupt = true;
                                }
                                let _ = send(
                                    &event_tx,
                                    AgentEvent::Done {
                                        status,
                                        result: None,
                                        error,
                                        session_id: Some(session_id.clone()),
                                    },
                                )
                                .await;
                            }
                            // EOF failed the pending prompt: the RPC-side
                            // message ("app-server exited before responding")
                            // hides the real evidence already in hand — the
                            // child's exit status and stderr tail. Re-read the
                            // child here so the surfaced Done carries both.
                            // An in-flight interrupt is excluded: the child
                            // died because we killed it, and the terminal
                            // bookkeeping below already ends that as
                            // Done{Interrupted}.
                            Ok(Err(rpc_err)) if !interrupted => {
                                done_current = true;
                                let status = child.try_wait().ok().flatten();
                                let detail =
                                    crate::crash_message(agent_name, status, &stderr_tail);
                                let _ = send(
                                    &event_tx,
                                    AgentEvent::Done {
                                        status: DoneStatus::Errored,
                                        result: None,
                                        error: Some(format!("{rpc_err}; {detail}")),
                                        session_id: Some(session_id.clone()),
                                    },
                                )
                                .await;
                            }
                            Err(_) if !interrupted => {
                                done_current = true;
                                let status = child.try_wait().ok().flatten();
                                let detail =
                                    crate::crash_message(agent_name, status, &stderr_tail);
                                let _ = send(
                                    &event_tx,
                                    AgentEvent::Done {
                                        status: DoneStatus::Errored,
                                        result: None,
                                        error: Some(format!(
                                            "{agent_name} exited mid-turn; {detail}"
                                        )),
                                        session_id: Some(session_id.clone()),
                                    },
                                )
                                .await;
                            }
                            _ => {}
                        }
                    }
                    break 'main;
                }
            },

            res = async { steering_call.as_mut().expect("guarded by if").1.as_mut().await },
                if steering_call.is_some() =>
            {
                let (text, _) = steering_call.take().expect("guarded by if");
                let outcome = match &res {
                    Ok(resp) => resp
                        .get("outcome")
                        .and_then(Value::as_str)
                        .unwrap_or("injected")
                        .to_owned(),
                    Err(e) => {
                        tracing::debug!(
                            target: "hearth_harness::acp",
                            "_session/steering failed (redelivering): {e}"
                        );
                        // Failed calls redeliver like a lost turn-end race.
                        "promptRequired".to_owned()
                    }
                };
                if interrupted {
                    // The run is winding down; the steer is abandoned like
                    // any queued steer at interrupt.
                } else if outcome != "promptRequired" {
                    // Injected into a live turn → a Steered boundary. But if
                    // the turn ended while the call was in flight, the
                    // injection was consumed by THAT turn — its output
                    // already streamed and the turn's Done already closed the
                    // segment. Emitting Steered after that Done re-armed the
                    // consumer (parked session → Working) with no next turn
                    // and no Done ever coming — the stranded-Working /
                    // eternal-timer bug. Post-turn: nothing left to do.
                    if turn.is_some() {
                        // The injection proves the turn is LIVE: any settle
                        // deadline armed off a cost frame that raced this
                        // response is invalid evidence.
                        starve_deadline = None;
                        // Pre-injection updates can still sit in `incoming`
                        // (responses bypass that queue): drain them into the
                        // CURRENT segment first, or text the agent streamed
                        // before the injection landed folds after the split —
                        // the transcript attributes it to the reply-to-steer.
                        let mut consumer_gone = false;
                        while let Ok(inc) = incoming.try_recv() {
                            match inc {
                                Incoming::Notification { method, params } => {
                                    let events =
                                        session_update_events(&method, &params, &session_id, &mut subagents);
                                    for ev in events {
                                        if !send(&event_tx, ev).await {
                                            consumer_gone = true;
                                            break;
                                        }
                                    }
                                }
                                Incoming::Request { id, method, params } => {
                                    for ev in handle_server_request_live(
                                        &client,
                                        id,
                                        &method,
                                        &params,
                                        &request_input,
                                        &open_questions,
                                    ) {
                                        if !send(&event_tx, ev).await {
                                            consumer_gone = true;
                                            break;
                                        }
                                    }
                                }
                                _ => {}
                            }
                            if consumer_gone {
                                break;
                            }
                        }
                        if consumer_gone {
                            break 'main;
                        }
                        let (prev, next) = rotate(&mut assistant_message_id);
                        if !send(
                            &event_tx,
                            AgentEvent::Steered {
                                assistant_message_id: Some(prev),
                                next_assistant_message_id: Some(next),
                            },
                        )
                        .await
                        {
                            break 'main;
                        }
                    }
                } else if turn.is_some() {
                    // Raced the turn end: redeliver at the boundary the
                    // loop is about to hit. `noRunningTurn` is stronger —
                    // the adapter says nothing is running while our prompt
                    // is still outstanding: the starved-turn signature. Arm
                    // the grace deadline; if the prompt's response does not
                    // land first, the recovery arm below settles the dead
                    // turn and promotes this steer.
                    if res
                        .as_ref()
                        .ok()
                        .and_then(|r| r.get("reason"))
                        .and_then(Value::as_str)
                        == Some("noRunningTurn")
                    {
                        tracing::warn!(
                            target: "hearth_harness::acp",
                            "steering answered noRunningTurn with a prompt \
                             outstanding; arming starved-turn recovery"
                        );
                        starve_deadline =
                            Some(tokio::time::Instant::now() + STARVE_GRACE);
                    }
                    queued_steers.push_back(text);
                } else {
                    // The turn ended while the call was in flight and its
                    // boundary already passed — the steer becomes the next
                    // turn directly.
                    let (prev, next) = rotate(&mut assistant_message_id);
                    if !send(
                        &event_tx,
                        AgentEvent::Steered {
                            assistant_message_id: Some(prev),
                            next_assistant_message_id: Some(next),
                        },
                    )
                    .await
                    {
                        break 'main;
                    }
                    done_current = false;
                    turn_content_seen = false;
                    open_tools.clear();
                    had_exec = false;
                    last_update_at = tokio::time::Instant::now();
                    prompt_seq += 1;
                    current_prompt_id =
                        prompt_complete_extension.then(|| format!("hearth-p{prompt_seq}"));
                    prompt_stall_deadline =
                        prompt_stall.map(|d| tokio::time::Instant::now() + d);
                    turn = Some(prompt_turn(
                        client.clone(),
                        session_id.clone(),
                        text,
                        current_prompt_id.clone(),
                    ));
                }
                while let Some(next_text) = steer_backlog.pop_front() {
                    if turn.is_some() && !interrupted {
                        let fut = steering_call_future(&client, &session_id, &next_text);
                        steering_call = Some((next_text, fut));
                        break;
                    }
                    // No live turn to inject into: boundary delivery.
                    queued_steers.push_back(next_text);
                }
            },

            // Busy-session cancel flushed (see BUSY_RECENT/CANCEL_FLUSH
            // above): the unowned self-continued turn had its cancel and a
            // drain window; the queued steer becomes a fresh prompt on a
            // now-idle agent.
            _ = tokio::time::sleep_until(
                cancel_flush_deadline.unwrap_or_else(tokio::time::Instant::now)
            ), if cancel_flush_deadline.is_some() && !interrupted => {
                cancel_flush_deadline = None;
                if turn.is_none()
                    && let Some(text) = queued_steers.pop_front()
                {
                    let (prev, next) = rotate(&mut assistant_message_id);
                    if !send(
                        &event_tx,
                        AgentEvent::Steered {
                            assistant_message_id: Some(prev),
                            next_assistant_message_id: Some(next),
                        },
                    )
                    .await
                    {
                        break 'main;
                    }
                    done_current = false;
                    turn_content_seen = false;
                    open_tools.clear();
                    had_exec = false;
                    last_update_at = tokio::time::Instant::now();
                    prompt_seq += 1;
                    current_prompt_id =
                        prompt_complete_extension.then(|| format!("hearth-p{prompt_seq}"));
                    prompt_stall_deadline =
                        prompt_stall.map(|d| tokio::time::Instant::now() + d);
                    turn = Some(prompt_turn(
                        client.clone(),
                        session_id.clone(),
                        text,
                        current_prompt_id.clone(),
                    ));
                } else if turn.is_none() && !steering_open {
                    // Mailbox closed while the flush waited: nothing left.
                    break 'main;
                }
            },

            // BLANKET quiet settle (see `quiet_settle` above), adapter-
            // agnostic: content streamed, every tool resolved, no question
            // pending, stream quiet past the window with the prompt still
            // unsettled. Feeds the recovery arm below by expiring its
            // deadline — one settle path for all three evidence sources.
            _ = tokio::time::sleep_until(
                last_update_at + {
                    let base = quiet_settle.unwrap_or_default();
                    if had_exec {
                        exec_quiet.map(|d| d.max(base)).unwrap_or(base)
                    } else {
                        base
                    }
                }
            ), if quiet_settle.is_some()
                && starve_deadline.is_none()
                && turn.is_some()
                && !interrupted
                && turn_content_seen
                && open_tools.is_empty()
                && open_questions.load(std::sync::atomic::Ordering::SeqCst) == 0 =>
            {
                let base = quiet_settle.unwrap_or_default();
                let window = if had_exec {
                    exec_quiet.map(|d| d.max(base)).unwrap_or(base)
                } else {
                    base
                };
                tracing::warn!(
                    target: "hearth_harness::acp",
                    quiet_ms = window.as_millis() as u64,
                    had_exec,
                    "turn quiet past the settle window with completed output; \
                     treating the prompt response as dropped"
                );
                starve_deadline = Some(tokio::time::Instant::now());
            },

            // Starved-turn recovery: the grace elapsed with the prompt still
            // unsettled after turn-end evidence — the turn's terminal cost
            // frame (COST_HINT_GRACE, ~immediate), a steering call answered
            // noRunningTurn (STARVE_GRACE), or the blanket quiet settle
            // above. Close the dead turn out
            // with a Done — its output already streamed as session/updates
            // and its text was delivered via the CLI's own queue — then
            // promote any queued steer to a fresh prompt, which settles
            // normally on a now-idle agent (verified against the real
            // adapter).
            _ = tokio::time::sleep_until(
                starve_deadline.unwrap_or_else(tokio::time::Instant::now)
            ), if starve_deadline.is_some() && turn.is_some() && !interrupted => {
                starve_deadline = None;
                tracing::warn!(
                    target: "hearth_harness::acp",
                    "prompt response missing past turn-end evidence; settling \
                     the dead turn (and promoting any queued steer)"
                );
                // Drop the dead future: a response that somehow arrives later
                // resolves a closed channel harmlessly.
                turn = None;
                let (prev, _next) = rotate(&mut assistant_message_id);
                if !send(
                    &event_tx,
                    AgentEvent::AssistantMessageCompleted { assistant_message_id: prev },
                )
                .await
                {
                    break 'main;
                }
                done_current = true;
                if !send(
                    &event_tx,
                    AgentEvent::Done {
                        status: DoneStatus::Completed,
                        result: None,
                        error: None,
                        session_id: Some(session_id.clone()),
                    },
                )
                .await
                {
                    break 'main;
                }
                if let Some(text) = queued_steers.pop_front() {
                    let (prev, next) = rotate(&mut assistant_message_id);
                    if !send(
                        &event_tx,
                        AgentEvent::Steered {
                            assistant_message_id: Some(prev),
                            next_assistant_message_id: Some(next),
                        },
                    )
                    .await
                    {
                        break 'main;
                    }
                    done_current = false;
                    turn_content_seen = false;
                    open_tools.clear();
                    had_exec = false;
                    last_update_at = tokio::time::Instant::now();
                    prompt_seq += 1;
                    current_prompt_id =
                        prompt_complete_extension.then(|| format!("hearth-p{prompt_seq}"));
                    prompt_stall_deadline =
                        prompt_stall.map(|d| tokio::time::Instant::now() + d);
                    turn = Some(prompt_turn(
                        client.clone(),
                        session_id.clone(),
                        text,
                        current_prompt_id.clone(),
                    ));
                } else if !steering_open {
                    // Mirror the normal turn-settled exit: mailbox closed
                    // and nothing left to run — the session is over.
                    break 'main;
                }
            },

            steer = steering.recv(), if steering_open && !interrupted => match steer {
                Some(msg) => {
                    // Same transform as the initial prompt: Claude's
                    // Ultrathink prefix rides every steer too.
                    let text = prompt_transform(request.reasoning, &msg.prompt);
                    if turn.is_none() && cancel_flush_deadline.is_some() {
                        // A busy-session cancel is already in flight: this
                        // steer lines up behind it and dispatches at flush.
                        queued_steers.push_back(text);
                    } else if turn.is_none()
                        && (!open_tools.is_empty()
                            || last_update_at.elapsed() < BUSY_RECENT)
                    {
                        // Mid self-continued turn (see BUSY_RECENT above):
                        // cancel it rather than prompt into the starve.
                        //
                        // Claude skips this branch ON PURPOSE and prompts
                        // straight in — its NATIVE semantics: the CLI queues
                        // the message and folds it into the running turn (no
                        // work lost, verified from live session data). The
                        // adapter drops that prompt's reply, and the
                        // cost-frame settle reconstructs it ~1s after the
                        // merged turn really ends. Only adapters with no
                        // verified turn-end frame pay the cancel.
                        tracing::info!(
                            target: "hearth_harness::acp",
                            "steer into a self-continuing session; cancelling \
                             the unowned turn before prompting"
                        );
                        client.notify(
                            "session/cancel",
                            Some(json!({ "sessionId": session_id })),
                        );
                        queued_steers.push_back(text);
                        cancel_flush_deadline =
                            Some(tokio::time::Instant::now() + CANCEL_FLUSH);
                    } else if turn.is_none() {
                        // Idle between turns: a steer is simply the next turn.
                        let (prev, next) = rotate(&mut assistant_message_id);
                        if !send(
                            &event_tx,
                            AgentEvent::Steered {
                                assistant_message_id: Some(prev),
                                next_assistant_message_id: Some(next),
                            },
                        )
                        .await
                        {
                            break 'main;
                        }
                        done_current = false;
                        turn_content_seen = false;
                        open_tools.clear();
                        had_exec = false;
                        last_update_at = tokio::time::Instant::now();
                        prompt_seq += 1;
                    current_prompt_id =
                        prompt_complete_extension.then(|| format!("hearth-p{prompt_seq}"));
                    prompt_stall_deadline =
                        prompt_stall.map(|d| tokio::time::Instant::now() + d);
                    turn = Some(prompt_turn(
                        client.clone(),
                        session_id.clone(),
                        text,
                        current_prompt_id.clone(),
                    ));
                    } else if steer_ext {
                        // Mid-turn injection via the `_session/steering`
                        // extension: start the call, resolved by its own
                        // select branch. One call in flight at a time;
                        // followers wait in the backlog.
                        if steering_call.is_some() {
                            steer_backlog.push_back(text);
                        } else {
                            let fut = steering_call_future(&client, &session_id, &text);
                            steering_call = Some((text, fut));
                        }
                    } else {
                        // No extension (Grok today): turn-boundary delivery.
                        queued_steers.push_back(text);
                    }
                }
                None => {
                    steering_open = false;
                    if turn.is_none() && queued_steers.is_empty() {
                        break 'main;
                    }
                }
            },

            _ = interrupt.cancelled(), if !interrupt_sent => {
                interrupt_sent = true;
                interrupted = true;
                if turn.is_some() {
                    client.notify("session/cancel", Some(json!({ "sessionId": session_id })));
                    // Escalate if the agent doesn't wind down (stopReason
                    // "cancelled") within the grace periods.
                    if let Some(pid) = child.id() {
                        escalation = Some(tokio::spawn(async move {
                            tokio::time::sleep(interrupt_grace).await;
                            send_signal(pid, Signal::Term);
                            tokio::time::sleep(kill_grace).await;
                            send_signal(pid, Signal::Kill);
                        }));
                    }
                } else {
                    // Idle between turns: nothing to cancel — the terminal
                    // bookkeeping below still guarantees Done { Interrupted }.
                    break 'main;
                }
            },

            // Prompt-stall watchdog: a prompt was sent and NOTHING has come
            // back on the wire at all — no queue bookkeeping, no updates, no
            // requests. Healthy grok acknowledges within milliseconds; total
            // silence past the bound is a wedged agent (stale shared leader,
            // launch-time update check). Surface a visible error instead of
            // indefinite Working.
            _ = tokio::time::sleep_until(
                prompt_stall_deadline.unwrap_or_else(tokio::time::Instant::now),
            ), if prompt_stall_deadline.is_some() && turn.is_some() && !interrupted => {
                let _ = send(
                    &event_tx,
                    AgentEvent::Error {
                        message: format!(
                            "{agent_name} did not respond to the prompt at all \
                             (no wire activity for {}s). {}",
                            prompt_stall.map(|d| d.as_secs()).unwrap_or(0),
                            stall_hint,
                        ),
                    },
                )
                .await;
                done_current = true;
                let _ = send(
                    &event_tx,
                    AgentEvent::Done {
                        status: DoneStatus::Errored,
                        result: None,
                        error: Some(format!(
                            "{agent_name} is unresponsive — the run was closed."
                        )),
                        session_id: Some(session_id.clone()),
                    },
                )
                .await;
                break 'main;
            },

            _ = event_tx.closed() => break 'main,
        }
    }

    // Terminal bookkeeping: never end the stream without a Done unless the
    // consumer already hung up.
    if !event_tx.is_closed() {
        if interrupted && !done_after_interrupt {
            let _ = event_tx
                .send(Ok(AgentEvent::Done {
                    status: DoneStatus::Interrupted,
                    result: None,
                    error: None,
                    session_id: Some(session_id.clone()),
                }))
                .await;
        } else if !interrupted && !done_current {
            // A child killed mid-turn must not read as a silent success.
            let status = child.try_wait().ok().flatten();
            let _ = event_tx
                .send(Ok(AgentEvent::Done {
                    status: DoneStatus::Errored,
                    result: None,
                    error: Some(crate::crash_message(agent_name, status, &stderr_tail)),
                    session_id: Some(session_id.clone()),
                }))
                .await;
        }
    }

    // Escalation dies BEFORE the child is reaped: after `shutdown_child`
    // waits the pid, a still-armed SIGTERM/SIGKILL timer would fire at a
    // freed (reusable) pid.
    if let Some(handle) = escalation {
        handle.abort();
    }
    shutdown_child(&mut child, kill_grace).await;
}
