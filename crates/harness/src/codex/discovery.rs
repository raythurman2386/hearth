//! The short-lived `codex app-server` probe behind [`Harness::commands`]:
//! handshake, then `skills/list`.
//!
//! [`Harness::commands`]: crate::Harness::commands

use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::process::Command;

use hearth_proto::SlashCommand;

use crate::HarnessError;
use crate::jsonrpc::RpcClient;
use crate::shutdown_child;

use super::CodexHarness;

impl CodexHarness {
    /// Short-lived discovery probe: a `codex app-server` handshake followed by
    /// `skills/list` — the only invocable-listing method the 0.146.x wire has
    /// (custom `~/.codex/prompts` are NOT exposed; the TUI-only built-ins
    /// aren't either). Skills are what the codex TUI itself surfaces as
    /// slash-invocables, listed per-cwd and deduped by name here.
    pub(crate) async fn discover_commands(&self) -> Result<Vec<SlashCommand>, HarnessError> {
        let exe = self.resolve_executable()?;
        let mut cmd = Command::new(&exe);
        cmd.arg("app-server");
        crate::compose_child_path(&mut cmd, &exe);
        crate::isolate_process_group(&mut cmd);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                HarnessError::NotInstalled(exe.display().to_string())
            } else {
                HarnessError::Io(e)
            }
        })?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            shutdown_child(&mut child, self.kill_grace).await;
            return Err(HarnessError::Protocol("codex child has no stdio".into()));
        };
        // The receiver must stay alive for the client's reader loop; agent →
        // client traffic during the probe is ignored.
        let (client, _incoming) = RpcClient::new(stdin, stdout);
        let discovery = async {
            client
                .request(
                    "initialize",
                    json!({
                        "clientInfo": {
                            "name": "hearth-native",
                            "title": "Hearth",
                            "version": env!("CARGO_PKG_VERSION"),
                        },
                        "capabilities": { "experimentalApi": true },
                    }),
                )
                .await?;
            client.notify("initialized", None);
            let skills = client.request("skills/list", json!({})).await?;
            Ok::<Vec<SlashCommand>, HarnessError>(parse_skill_commands(&skills))
        };
        let result = tokio::time::timeout(Duration::from_secs(10), discovery).await;
        shutdown_child(&mut child, self.kill_grace).await;
        match result {
            Ok(inner) => inner,
            Err(_) => Err(HarnessError::Protocol("command discovery timed out".into())),
        }
    }
}

/// `skills/list` result → picker commands. `data` groups skills by cwd; the
/// same skill appears under every root, so dedupe by name keeping first
/// appearance order. The interface's shortDescription is picker-sized; the
/// top-level description is a model-facing paragraph, kept only as fallback.
fn parse_skill_commands(result: &Value) -> Vec<SlashCommand> {
    let mut seen = std::collections::HashSet::new();
    let mut commands = Vec::new();
    for group in result
        .get("data")
        .and_then(Value::as_array)
        .map(|a| a.as_slice())
        .unwrap_or_default()
    {
        for skill in group
            .get("skills")
            .and_then(Value::as_array)
            .map(|a| a.as_slice())
            .unwrap_or_default()
        {
            let Some(name) = skill
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|n| !n.is_empty())
            else {
                continue;
            };
            if !seen.insert(name.to_owned()) {
                continue;
            }
            let interface = skill.get("interface");
            let description = interface
                .and_then(|i| i.get("shortDescription"))
                .and_then(Value::as_str)
                .filter(|d| !d.is_empty())
                .or_else(|| skill.get("description").and_then(Value::as_str))
                .unwrap_or_default();
            commands.push(SlashCommand {
                name: name.to_owned(),
                description: description.to_owned(),
                input_hint: None,
            });
        }
    }
    commands
}
