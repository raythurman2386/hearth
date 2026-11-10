//! Handshake + session setup: `initialize`, `session/new` (or `session/load`
//! with a fresh-session fallback), the run's mode/model/effort switches, and
//! the events that open the run. On failure the terminal Done is emitted and
//! the child reaped here, so the caller has nothing left to do.

use std::collections::VecDeque;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::mpsc;

use hearth_proto::{AgentEvent, DoneStatus, HarnessId, ReasoningLevel, RunRequest, SlashCommand};

use crate::jsonrpc::{Incoming, RpcClient};
use crate::{HarnessError, RunControls, shutdown_child};

use super::super::config::{config_option_sets, first_class_model_change};
use super::super::discovery::scan_available_commands;
use super::super::new_message_id;
use super::super::session::Session;
use super::super::session::{initialize_params, steering_supported};
use super::super::subagent::SubagentTracker;
use super::super::turn::{SubagentObserver, prompt_turn, request_draining};
use super::RunState;
use super::send;

/// The handshake proper: `initialize` → `session/new` (or `session/load` with
/// a fresh-session fallback) → the run's mode/model/effort/option switches.
/// Returns the session id, whether the agent supports mid-turn steering, and
/// the commands the handshake advertised.
async fn handshake(
    client: &RpcClient,
    incoming: &mut mpsc::Receiver<Incoming>,
    request: &RunRequest,
    harness: HarnessId,
    effort_values: fn(Option<ReasoningLevel>, Option<&str>) -> Vec<&'static str>,
) -> Result<(String, bool, Vec<SlashCommand>), HarnessError> {
    let init = client
        .request("initialize", initialize_params(harness))
        .await?;
    let steer_ext = steering_supported(&init);
    let init_commands = scan_available_commands(&init);

    let session_params = json!({ "cwd": request.cwd, "mcpServers": [] });
    let (session_id, session_response) = if let Some(resume) = &request.resume {
        let mut load = session_params.clone();
        load["sessionId"] = Value::String(resume.clone());
        match request_draining(client, incoming, "session/load", load).await {
            Ok(resp) => (resume.clone(), resp),
            // A missing/foreign session falls back to a fresh one.
            Err(e) => {
                tracing::debug!(
                    target: "hearth_harness::acp",
                    "session/load failed (starting fresh): {e}"
                );
                let new = request_draining(client, incoming, "session/new", session_params.clone())
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
        let new = request_draining(client, incoming, "session/new", session_params).await?;
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
            client,
            incoming,
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
    if let Some(model) = first_class_model_change(&session_response, request.model.as_deref())? {
        request_draining(
            client,
            incoming,
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
            client,
            incoming,
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
    Ok::<(String, bool, Vec<SlashCommand>), HarnessError>((session_id, steer_ext, init_commands))
}

/// Run the interruptible handshake and open the session. `None` means the run
/// is already over: the terminal Done was emitted (a setup failure or an
/// interrupt before the handshake finished) and the child reaped.
pub(super) async fn setup(session: Session) -> Option<RunState> {
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
        steering,
        interrupt,
    } = controls;
    let request_input = std::sync::Arc::new(request_input);

    // ---- handshake + session (interruptible) ------------------------------
    let setup = handshake(&client, &mut incoming, &request, harness, effort_values);
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
                    return None;
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
            return None;
        }
    };

    let assistant_message_id = new_message_id();
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
        return None;
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
        return None;
    }

    // Subagent correlation + transcript tails: the grok tracker (inert for
    // agents that never emit the `subagent_*` extension updates).
    let subagents = SubagentObserver(SubagentTracker::new(
        session_id.clone(),
        event_tx.clone(),
        sessions_root,
    ));

    // ---- main loop state --------------------------------------------------
    // Prompt-completion settlement state (the prompt-complete extension):
    // one prompt is outstanding at a time, identified by `current_prompt_id`;
    // settled ids are remembered so a STALE `prompt_complete` (a late replay
    // of an already-settled prompt) can never settle a newer turn.
    let mut prompt_seq: u64 = 0;
    let completed_prompts: VecDeque<String> = VecDeque::new();
    // `HEARTH_ACP_PROMPT_STALL_MS` overrides the spec's bound; 0 disables.
    let prompt_stall: Option<Duration> = match std::env::var("HEARTH_ACP_PROMPT_STALL_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        Some(0) => None,
        Some(ms) => Some(Duration::from_millis(ms)),
        None => prompt_stall,
    };
    let prompt_stall_deadline = prompt_stall.map(|d| tokio::time::Instant::now() + d);
    prompt_seq += 1;
    let current_prompt_id = prompt_complete_extension.then(|| format!("hearth-p{prompt_seq}"));
    let turn = Some(prompt_turn(
        client.clone(),
        session_id.clone(),
        prompt_transform(request.reasoning, &request.prompt),
        current_prompt_id.clone(),
    ));
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

    Some(RunState {
        child,
        client,
        incoming,
        event_tx,
        steering,
        interrupt,
        request_input,
        request,
        agent_name,
        prompt_transform,
        prompt_complete_extension,
        prompt_stall,
        stall_hint,
        interrupt_grace,
        kill_grace,
        stderr_tail,
        session_id,
        steer_ext,
        subagents,
        assistant_message_id,
        open_questions: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        prompt_seq,
        current_prompt_id,
        completed_prompts,
        prompt_stall_deadline,
        turn,
        // Steers waiting for the turn boundary (agents without the extension,
        // or extension steers that lost the turn-end race).
        queued_steers: VecDeque::new(),
        // The in-flight `_session/steering` call (text + response future),
        // plus followers awaiting their turn. Polled from the main select so
        // the loop keeps draining `incoming` while the agent responds —
        // awaiting inline deadlocks against a full incoming channel when the
        // agent floods updates (the reader blocks on the channel and never
        // parses the steering response).
        steering_call: None,
        steer_backlog: VecDeque::new(),
        steering_open: true,
        interrupted: false,
        interrupt_sent: false,
        done_current: false,
        done_after_interrupt: false,
        escalation: None,
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
        // the queued steer is promoted to a fresh turn. See `steering.rs` for
        // the grace itself.
        starve_deadline: None,
        quiet_settle,
        exec_quiet,
        last_update_at: tokio::time::Instant::now(),
        turn_content_seen: false,
        open_tools: std::collections::HashSet::new(),
        had_exec: false,
        // PREVENTION, ahead of all the recovery above: never send a
        // `session/prompt` into a session that is visibly mid SELF-CONTINUED
        // turn — that prompt's reply is what the adapter drops (the verified
        // starve). Visibly busy = an open tool call, or stream traffic within
        // BUSY_RECENT, with no prompt of ours outstanding. The discipline is
        // Zed's, verified against the real adapter: `session/cancel` the
        // unowned turn, give it CANCEL_FLUSH to die and drain, then prompt.
        // This makes the interactive path starve-free; the settle layers below
        // remain for the notification race a client cannot see coming.
        cancel_flush_deadline: None,
    })
}
