//! One turn on the wire: the `session/prompt` call, the session-update
//! mapping, permission/question requests, and the usage/stop shaping that ends
//! a turn.

use futures::future::BoxFuture;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use hearth_proto::{AgentEvent, DoneStatus, ToolCall, UserInputAnswer, UserInputQuestion};

use crate::HarnessError;
use crate::jsonrpc::{Incoming, RpcClient};

use super::new_message_id;
use super::normalize::{map_update, preferred_allow_option};
use super::subagent::SubagentTracker;

/// The subagent correlator: grok's spawn-tool + disk-tail tracker (inert
/// for agents that never emit `subagent_*` updates). It observes the raw
/// updates ahead of [`map_update`]; its tagged events flow from its own
/// tasks.
pub(crate) struct SubagentObserver(pub(crate) SubagentTracker);

impl SubagentObserver {
    fn observe(&mut self, update: &Value) {
        self.0.observe(update)
    }
}

/// The events of one notification, session-filtered. `session/update` maps
/// per [`map_update`]; `_x.ai/session_notification` is grok's extension
/// channel — same `{sessionId, update}` envelope, but its updates (the
/// `subagent_*` lifecycle) render nothing directly. The subagent tracker sees
/// both first (spawn/finished correlation + transcript tails); its tagged
/// events flow from its own tasks, not this return value.
pub(crate) fn session_update_events(
    method: &str,
    params: &Value,
    session_id: &str,
    subagents: &mut SubagentObserver,
) -> Vec<AgentEvent> {
    if params.get("sessionId").and_then(Value::as_str) != Some(session_id) {
        return Vec::new();
    }
    let update = params.get("update").unwrap_or(&Value::Null);
    match method {
        "session/update" => {
            subagents.observe(update);
            map_update(update)
        }
        "_x.ai/session_notification" => {
            subagents.observe(update);
            Vec::new()
        }
        _ => Vec::new(),
    }
}

/// Per-turn token usage from a settled `session/prompt` response, when the
/// adapter attaches it (tolerant of both field spellings; absent → nothing).
pub(crate) fn usage_from_response(res: &Result<Value, HarnessError>) -> Option<AgentEvent> {
    let resp = res.as_ref().ok()?;
    // Grok settles usage on the response `_meta` (inputTokens/outputTokens —
    // verified live, 1.0.4); adapters used a first-class `usage` object.
    let usage = resp.get("usage").or_else(|| resp.get("_meta"))?;
    let count = |keys: &[&str]| {
        keys.iter()
            .find_map(|k| usage.get(*k))
            .and_then(Value::as_u64)
    };
    let input = count(&["inputTokens", "input_tokens"]);
    let output = count(&["outputTokens", "output_tokens"]);
    (input.is_some() || output.is_some()).then(|| AgentEvent::Usage {
        input_tokens: input.unwrap_or(0),
        output_tokens: output.unwrap_or(0),
    })
}

/// Map a finished `session/prompt` result to the run's terminal status.
pub(crate) fn stop_outcome(
    res: &Result<Value, HarnessError>,
    interrupted: bool,
) -> (DoneStatus, Option<String>) {
    if interrupted {
        return (DoneStatus::Interrupted, None);
    }
    match res {
        Ok(resp) => match resp.get("stopReason").and_then(Value::as_str) {
            Some("cancelled") => (DoneStatus::Interrupted, None),
            Some("refusal") => (
                DoneStatus::Errored,
                Some("The agent refused to continue.".to_owned()),
            ),
            // end_turn / max_tokens / max_turn_requests: the turn ended;
            // partial output is already in the doc.
            _ => (DoneStatus::Completed, None),
        },
        Err(e) => (DoneStatus::Errored, Some(e.to_string())),
    }
}

/// One turn: `session/prompt` whose response (the `stopReason`) ends it.
/// `prompt_id` (agents with the prompt-complete extension) rides `_meta` so
/// the `_x.ai/session/prompt_complete` notification can be matched exactly —
/// grok echoes it back (verified live, 1.0.4).
pub(crate) fn prompt_turn(
    client: RpcClient,
    session_id: String,
    text: String,
    prompt_id: Option<String>,
) -> BoxFuture<'static, Result<Value, HarnessError>> {
    Box::pin(async move {
        let mut params = json!({
            "sessionId": session_id,
            "prompt": [{ "type": "text", "text": text }],
        });
        if let Some(id) = prompt_id {
            params["_meta"] = json!({ "promptId": id, "requestId": id });
        }
        // A prompt's response IS the turn: its lifetime is owned by the
        // session loop's stall/quiet-settle watchdogs, not the generic
        // per-request bound.
        client.request_untimed("session/prompt", params).await
    })
}

/// Answer a server→client request. Permission requests are auto-accepted with
/// the agent's preferred allow option — parity with the claude harness's
/// bypassPermissions and the codex harness's approvalPolicy "never" (hearth
/// sessions run unattended). Everything else (fs, terminal, elicitation) was
/// declined at initialize, so a stray request gets method-not-found rather
/// than wedging the agent.
fn handle_server_request(
    client: &RpcClient,
    id: Value,
    method: &str,
    params: &Value,
) -> Vec<AgentEvent> {
    match method {
        "session/request_permission" => {
            let options: Vec<Value> = params
                .get("options")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            match preferred_allow_option(&options) {
                Some(option_id) => client.respond(
                    &id,
                    json!({ "outcome": { "outcome": "selected", "optionId": option_id } }),
                ),
                None => client.respond(&id, json!({ "outcome": { "outcome": "cancelled" } })),
            }
            Vec::new()
        }
        _ => {
            tracing::debug!(target: "hearth_harness::acp", "unhandled server request: {method}");
            client.respond_error(&id, -32601, &format!("unsupported method: {method}"));
            Vec::new()
        }
    }
}

pub(crate) type RequestInputFn = Box<
    dyn Fn(Vec<UserInputQuestion>) -> tokio::sync::oneshot::Receiver<Vec<UserInputAnswer>>
        + Send
        + Sync,
>;

/// A permission request is a QUESTION (not a tool permission) when any of
/// its options lacks an allow/reject kind — that's how the agent relays
/// user-facing choices (Claude's AskUserQuestion arrives this way through
/// the adapter). Every option carrying an allow/reject kind means a real
/// tool permission, which auto-accepts (unattended parity); kinds may
/// legitimately repeat — codex sends two `allow_always` options ("Allow for
/// Session" and a prefix-rule amendment) on every exec approval.
fn is_user_question(options: &[Value]) -> bool {
    options.iter().any(|option| {
        !matches!(
            option.get("kind").and_then(Value::as_str),
            Some("allow_once" | "allow_always" | "reject_once" | "reject_always")
        )
    })
}

/// The live-run request handler: tool permissions auto-accept like
/// [`handle_server_request`], but question-shaped requests block on the
/// engine's input bridge (in a subtask so the message loop keeps flowing)
/// and answer with the option whose name matches the chosen label. A dropped
/// resolver degrades to `cancelled` — never a silent allow.
pub(crate) fn handle_server_request_live(
    client: &RpcClient,
    id: Value,
    method: &str,
    params: &Value,
    request_input: &std::sync::Arc<RequestInputFn>,
    open_questions: &std::sync::Arc<std::sync::atomic::AtomicUsize>,
) -> Vec<AgentEvent> {
    if method != "session/request_permission" {
        return handle_server_request(client, id, method, params);
    }
    let options: Vec<Value> = params
        .get("options")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if !is_user_question(&options) {
        return handle_server_request(client, id, method, params);
    }
    let names: Vec<String> = options
        .iter()
        .map(|o| {
            o.get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        })
        .collect();
    let question = UserInputQuestion {
        id: new_message_id(),
        header: "Agent question".into(),
        question: params
            .get("toolCall")
            .and_then(|t| t.get("title"))
            .and_then(Value::as_str)
            .unwrap_or("The agent needs your input.")
            .to_owned(),
        options: names.clone(),
        multi_select: false,
    };
    let client = client.clone();
    let request_input = std::sync::Arc::clone(request_input);
    // Pending questions block the agent — the quiet-settle must not read
    // that silence as a finished turn.
    let open_questions = std::sync::Arc::clone(open_questions);
    open_questions.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    tokio::spawn(async move {
        let answers = (request_input)(vec![question.clone()])
            .await
            .unwrap_or_default();
        let picked = answers
            .iter()
            .find(|a| a.question_id == question.id)
            .and_then(|a| a.labels.first())
            .and_then(|label| {
                options
                    .iter()
                    .find(|o| o.get("name").and_then(Value::as_str) == Some(label.as_str()))
            })
            .and_then(|o| o.get("optionId").and_then(Value::as_str));
        match picked {
            Some(option_id) => client.respond(
                &id,
                json!({ "outcome": { "outcome": "selected", "optionId": option_id } }),
            ),
            None => client.respond(&id, json!({ "outcome": { "outcome": "cancelled" } })),
        }
        open_questions.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    });
    Vec::new()
}

/// Await a setup request while draining incoming messages, so a `session/load`
/// whose replay outruns the incoming channel's capacity can't deadlock the
/// reader. Replayed `session/update`s are dropped (the doc already holds the
/// history); server requests are answered.
pub(crate) async fn request_draining(
    client: &RpcClient,
    incoming: &mut mpsc::Receiver<Incoming>,
    method: &'static str,
    params: Value,
) -> Result<Value, HarnessError> {
    let mut fut = prompt_like_request(client.clone(), method, params);
    let res = loop {
        tokio::select! {
            res = &mut fut => break res,
            inc = incoming.recv() => match inc {
                Some(Incoming::Request { id, method, params }) => {
                    handle_server_request(client, id, &method, &params);
                }
                Some(_) => {}
                None => {
                    return Err(HarnessError::Protocol(format!(
                        "{method}: agent exited during setup"
                    )));
                }
            },
        }
    };
    // Responses resolve through the pending map, not the incoming queue, so
    // replay updates the reader forwarded BEFORE the response line may still
    // sit in the buffer — flush them now or they'd leak into the live turn.
    while let Ok(inc) = incoming.try_recv() {
        if let Incoming::Request { id, method, params } = inc {
            handle_server_request(client, id, &method, &params);
        }
    }
    res
}

fn prompt_like_request(
    client: RpcClient,
    method: &'static str,
    params: Value,
) -> BoxFuture<'static, Result<Value, HarnessError>> {
    // Prompt-shaped calls (`session/prompt`, `_session/steering`) are answered
    // at turn boundaries, not within a request round trip: no fixed bound.
    Box::pin(async move { client.request_untimed(method, params).await })
}

/// Track the liveness signals the blanket quiet-settle keys on: content
/// proves the turn produced something; an open tool call or a pending
/// question proves silence is legitimate.
pub(crate) fn track_turn_signals(
    ev: &AgentEvent,
    content_seen: &mut bool,
    open_tools: &mut std::collections::HashSet<String>,
    had_exec: &mut bool,
) {
    match ev {
        AgentEvent::TextDelta { text } if !text.is_empty() => *content_seen = true,
        AgentEvent::ToolCall { id, call, .. } => {
            *content_seen = true;
            open_tools.insert(id.clone());
            if matches!(call, ToolCall::Exec { .. }) {
                *had_exec = true;
            }
        }
        AgentEvent::ToolResult { id, .. } => {
            open_tools.remove(id);
        }
        _ => {}
    }
}

/// A mid-turn `_session/steering` call. `idleBehavior: promptRequired`
/// covers the turn-ended race: the agent hands the text back instead of
/// firing an untracked turn.
pub(crate) fn steering_call_future(
    client: &RpcClient,
    session_id: &str,
    text: &str,
) -> BoxFuture<'static, Result<Value, HarnessError>> {
    let params = json!({
        "sessionId": session_id,
        "prompt": [{ "type": "text", "text": text }],
        "_meta": { "steering": { "idleBehavior": "promptRequired" } },
    });
    prompt_like_request(client.clone(), "_session/steering", params)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_exec_approval_options_are_not_a_question() {
        // codex-acp's real exec-approval shape: two allow_always entries (the
        // session allow + a prefix-rule amendment). Must auto-accept.
        let options = vec![
            json!({ "optionId": "allow_once", "name": "Allow Once", "kind": "allow_once" }),
            json!({ "optionId": "allow_always", "name": "Allow for Session", "kind": "allow_always" }),
            json!({ "optionId": "allow_prefix", "name": "Allow Commands Starting With `cargo test`", "kind": "allow_always" }),
            json!({ "optionId": "reject", "name": "Reject", "kind": "reject_once" }),
        ];
        assert!(!is_user_question(&options));
        // AskUserQuestion relays choices without allow/reject kinds.
        let question = vec![
            json!({ "optionId": "a", "name": "Blue" }),
            json!({ "optionId": "b", "name": "Green" }),
        ];
        assert!(is_user_question(&question));
        let mixed = vec![
            json!({ "optionId": "a", "name": "Proceed", "kind": "allow_once" }),
            json!({ "optionId": "b", "name": "Другое", "kind": "other" }),
        ];
        assert!(is_user_question(&mixed));
    }
}
