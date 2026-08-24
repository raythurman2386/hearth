//! M5c integration: agent-account slot mechanics (Codex swap), uploads
//! chunk→commit→readback + path jail, chat auto-titling with the mock harness,
//! and the RPC dispatch for each new method over the memory transport.
//!
//! Account tests use explicit `AgentAccountsConfig` paths under a tempdir (never
//! the real `~/.codex`), so they are hermetic and parallel-safe.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64_URL;

use hearth_engine::{
    AgentAccounts, AgentAccountsConfig, EngineCore, HarnessRegistry, Repos, Uploads,
    worktree_branch_from_title,
};
use hearth_harness::mock::MockHarness;
use hearth_proto::{AgentAccountsSnapshot, AgentEvent, DoneStatus, HarnessId, SandboxLevel};
use hearth_rpc::methods;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// AgentAccounts wired to a temp Codex home.
fn test_accounts(root: &Path) -> (AgentAccounts, AgentAccountsConfig) {
    let config = AgentAccountsConfig {
        data_dir: root.join("data"),
        codex_home: root.join("codex"),
    };
    (AgentAccounts::new(config.clone()), config)
}

/// An unsigned JWT with the claims codex mines from `id_token`.
fn fake_id_token(email: &str, account_id: &str, plan: &str) -> String {
    let header = BASE64_URL.encode(br#"{"alg":"none"}"#);
    let payload = BASE64_URL.encode(
        serde_json::json!({
            "email": email,
            "name": "Codex User",
            "https://api.openai.com/auth": {
                "chatgpt_account_id": account_id,
                "chatgpt_plan_type": plan,
            },
        })
        .to_string(),
    );
    format!("{header}.{payload}.x")
}

fn write_codex_login(config: &AgentAccountsConfig, email: &str, account_id: &str) {
    std::fs::create_dir_all(&config.codex_home).expect("codex home");
    std::fs::write(
        config.codex_home.join("auth.json"),
        serde_json::json!({
            "tokens": {
                "id_token": fake_id_token(email, account_id, "plus"),
                "access_token": format!("at-{account_id}"),
                "account_id": account_id,
            }
        })
        .to_string(),
    )
    .expect("codex auth");
}

fn account_emails(snapshot: &AgentAccountsSnapshot, harness: HarnessId) -> Vec<(String, bool)> {
    snapshot
        .accounts
        .iter()
        .filter(|a| a.harness == harness)
        .map(|a| (a.email.clone().unwrap_or_default(), a.active))
        .collect()
}

fn assemble_with_mock(dir: &Path, script: Vec<AgentEvent>) -> EngineCore {
    std::fs::create_dir_all(dir).expect("data dir");
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(MockHarness { script }));
    EngineCore::assemble(dir, Arc::new(registry), HarnessId::Mock, None).expect("engine assembles")
}

async fn git(cwd: &Path, args: &[&str]) {
    let output = tokio::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@test")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@test")
        .output()
        .await
        .expect("git spawns");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

async fn init_repo(dir: &Path) {
    std::fs::create_dir_all(dir).expect("repo dir");
    git(dir, &["init", "-b", "main"]).await;
    std::fs::write(dir.join("a.txt"), "one\n").expect("write a.txt");
    git(dir, &["add", "."]).await;
    git(dir, &["commit", "-m", "initial"]).await;
}

/// Poll until `probe` yields Some, or panic at the deadline.
async fn wait_for<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(value) = probe() {
            return value;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ---------------------------------------------------------------------------
// Agent accounts — Codex slot swap
// ---------------------------------------------------------------------------

#[tokio::test]
async fn codex_slot_swap_and_api_key_detection() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (accounts, config) = test_accounts(tmp.path());

    write_codex_login(&config, "carol@example.com", "acct-carol");
    let snapshot = accounts.list(false).await.expect("list");
    let carol = snapshot
        .accounts
        .iter()
        .find(|a| a.harness == HarnessId::Codex)
        .expect("codex account");
    assert_eq!(carol.email.as_deref(), Some("carol@example.com"));
    assert_eq!(carol.plan_label.as_deref(), Some("ChatGPT Plus"));
    assert!(carol.active);
    let carol_id = carol.id.clone();

    // Second login (Dave) becomes live; swap back to Carol.
    write_codex_login(&config, "dave@example.com", "acct-dave");
    accounts.list(false).await.expect("list dave");
    let snapshot = accounts
        .activate(HarnessId::Codex, &carol_id)
        .await
        .expect("activate carol");
    let mut emails = account_emails(&snapshot, HarnessId::Codex);
    emails.sort();
    assert_eq!(
        emails,
        vec![
            ("carol@example.com".to_string(), true),
            ("dave@example.com".to_string(), false)
        ]
    );
    let auth: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(config.codex_home.join("auth.json")).expect("auth"),
    )
    .expect("auth json");
    assert_eq!(auth["tokens"]["account_id"], "acct-carol");

    // API-key mode: no tokens, just the key.
    std::fs::write(
        config.codex_home.join("auth.json"),
        serde_json::json!({ "OPENAI_API_KEY": "sk-test-12345678abcd" }).to_string(),
    )
    .expect("api key auth");
    let snapshot = accounts.list(false).await.expect("list api key");
    let key_account = snapshot
        .accounts
        .iter()
        .find(|a| a.harness == HarnessId::Codex && a.active)
        .expect("api key account");
    assert_eq!(key_account.plan_label.as_deref(), Some("API key"));
    assert_eq!(key_account.email.as_deref(), Some("API key ·…abcd"));
}

#[tokio::test]
async fn forget_guards_and_removes_slots() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (accounts, config) = test_accounts(tmp.path());
    write_codex_login(&config, "alice@example.com", "acct-alice");
    let snapshot = accounts.list(false).await.expect("list");
    let alice_id = snapshot.accounts[0].id.clone();

    // Path-shaped ids never reach the filesystem.
    assert!(
        accounts
            .forget(HarnessId::Codex, "../../evil")
            .await
            .is_err()
    );
    assert!(
        accounts
            .forget(HarnessId::Codex, "ABCDEF0123456789")
            .await
            .is_err()
    );
    // The live login can't be forgotten (it would just be re-detected).
    assert!(accounts.forget(HarnessId::Codex, &alice_id).await.is_err());

    // A non-active slot forgets cleanly.
    write_codex_login(&config, "bob@example.com", "acct-bob");
    accounts.list(false).await.expect("list bob");
    let snapshot = accounts
        .forget(HarnessId::Codex, &alice_id)
        .await
        .expect("forget alice");
    assert_eq!(
        account_emails(&snapshot, HarnessId::Codex),
        vec![("bob@example.com".to_string(), true)]
    );
}

#[test]
fn snapshot_wire_shape() {
    let snapshot = AgentAccountsSnapshot::default();
    let value = serde_json::to_value(&snapshot).expect("serializes");
    assert_eq!(value, serde_json::json!({ "accounts": [], "warnings": [] }));
}

#[tokio::test]
async fn non_codex_login_is_refused() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (accounts, _) = test_accounts(tmp.path());
    assert!(accounts.start_login(HarnessId::Raven).await.is_err());
    assert!(
        accounts
            .complete_login("no-such-login", "code#state")
            .await
            .is_err()
    );
}

// ---------------------------------------------------------------------------
// Uploads
// ---------------------------------------------------------------------------

#[tokio::test]
async fn uploads_chunk_commit_readback_and_jail() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let uploads = Uploads::new(tmp.path());

    // 100KB of pseudo-random bytes, staged as three positional base64 chunks
    // (out of order, with one retried) — chunk boundaries are multiples of 3
    // bytes so independent base64 strings concatenate losslessly.
    let payload: Vec<u8> = (0..100_002u32)
        .map(|i| (i.wrapping_mul(31) % 251) as u8)
        .collect();
    let chunks: Vec<String> = payload.chunks(45_000).map(|c| BASE64.encode(c)).collect();
    assert_eq!(chunks.len(), 3);
    uploads
        .append("up-1", &chunks[2], Some(2))
        .expect("chunk 2");
    uploads
        .append("up-1", &chunks[0], Some(0))
        .expect("chunk 0");
    uploads
        .append("up-1", &chunks[0], Some(0))
        .expect("chunk 0 retry is idempotent");
    uploads
        .append("up-1", &chunks[1], Some(1))
        .expect("chunk 1");
    let path = uploads.commit("up-1", "photo.png").expect("commit");
    assert!(path.ends_with("up-1-photo.png"), "path: {path}");
    assert_eq!(std::fs::read(&path).expect("committed file"), payload);

    // Readback: chunked reassembly round-trips.
    let mut assembled = Vec::new();
    let mut offset = 0u64;
    loop {
        let chunk = uploads.read_chunk(&path, offset, &[]).expect("read chunk");
        assert_eq!(chunk.mime_type, "image/png");
        assert_eq!(chunk.name, "up-1-photo.png");
        assembled.extend(BASE64.decode(&chunk.data).expect("chunk base64"));
        offset = chunk.next_offset;
        if chunk.done {
            break;
        }
    }
    assert_eq!(assembled, payload);

    // Missing chunk → commit fails.
    uploads
        .append("up-2", &chunks[0], Some(0))
        .expect("chunk 0");
    uploads
        .append("up-2", &chunks[2], Some(2))
        .expect("chunk 2 (hole at 1)");
    assert!(
        uploads.commit("up-2", "holey.png").is_err(),
        "hole detected"
    );

    // Path jail: files outside the uploads dir (and outside any allowed cwd
    // root) are rejected, including traversal attempts and the dir itself.
    let outside = tmp.path().join("outside.png");
    std::fs::write(&outside, b"nope").expect("outside file");
    assert!(
        uploads
            .read_chunk(&outside.to_string_lossy(), 0, &[])
            .is_err()
    );
    assert!(uploads.read_chunk("/etc/passwd", 0, &[]).is_err());
    let sneaky = format!("{}/../outside.png", uploads.dir().display());
    assert!(
        uploads.read_chunk(&sneaky, 0, &[]).is_err(),
        "traversal rejected"
    );
    // …but a workspace-known cwd root admits its files.
    let ok = uploads
        .read_chunk(&outside.to_string_lossy(), 0, &[tmp.path().to_path_buf()])
        .expect("cwd-rooted read");
    assert_eq!(BASE64.decode(&ok.data).expect("data"), b"nope");
    // Non-image extensions are refused even inside the jail (hearth parity).
    let text = PathBuf::from(uploads.dir()).join("notes.txt");
    std::fs::create_dir_all(uploads.dir()).expect("uploads dir");
    std::fs::write(&text, b"text").expect("txt");
    assert!(uploads.read_chunk(&text.to_string_lossy(), 0, &[]).is_err());

    // Bogus upload ids never become paths.
    assert!(uploads.append("../evil", "aGk=", None).is_err());
    assert!(uploads.commit("unknown-upload", "x.png").is_err());
}

// ---------------------------------------------------------------------------
// Titling
// ---------------------------------------------------------------------------

#[tokio::test]
async fn titling_e2e_names_chat_and_renames_worktree_branch() {
    let tmp = tempfile::tempdir().expect("tempdir");
    // Worktree root must be inside the tempdir (EngineCore reads the env-less
    // default otherwise) — create the worktree with a dedicated Repos handle.
    let repo_dir = tmp.path().join("repo");
    init_repo(&repo_dir).await;
    let repos = Repos::with_worktrees_root(
        &tmp.path().join("data"),
        "device-test",
        tmp.path().join("worktrees"),
    );
    let worktree = repos
        .create_worktree(&repo_dir, "main")
        .await
        .expect("worktree");

    let core = assemble_with_mock(
        &tmp.path().join("data"),
        vec![
            AgentEvent::TextDelta {
                text: "Fix Login Flow".into(),
            },
            AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: None,
            },
        ],
    );
    let chat_id = "chat-title-1";
    core.workspace
        .create_space(
            "space-title",
            &core.device_id,
            &repo_dir.to_string_lossy(),
            None,
            true,
        )
        .expect("create space");
    core.workspace
        .create_chat(
            chat_id,
            Some("space-title"),
            None,
            None,
            Some(worktree.path.clone()),
        )
        .expect("create chat");
    core.workspace
        .set_chat_branch(chat_id, &worktree.branch)
        .expect("set branch");

    let request = hearth_proto::RunRequest {
        mode: None,
        prompt: "please fix the login flow".into(),
        harness: None,
        model: None,
        reasoning: None,
        model_options: serde_json::Map::new(),
        cwd: worktree.path.clone(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        attachments: Vec::new(),
        worktree: None,
        resume: None,
    };
    core.sessions
        .dispatch(chat_id, HarnessId::Mock, request, None)
        .await
        .expect("dispatch");

    // The mock's scripted reply doubles as the titling model's output.
    let chat = wait_for("chat title", || {
        core.workspace
            .chat(chat_id)
            .ok()
            .flatten()
            .filter(|c| c.title.as_deref().is_some_and(|t| !t.is_empty()))
    })
    .await;
    assert_eq!(chat.title.as_deref(), Some("Fix Login Flow"));
    // Branch renamed from the title, chat row updated to match.
    assert_eq!(chat.branch.as_deref(), Some("hearth/fix-login-flow"));
    let head = tokio::process::Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(&worktree.path)
        .output()
        .await
        .expect("git");
    assert_eq!(
        String::from_utf8_lossy(&head.stdout).trim(),
        "hearth/fix-login-flow"
    );

    // A titled chat is never re-titled: rename, run again, title sticks.
    core.workspace
        .rename_chat(chat_id, "My Custom Name")
        .expect("rename");
    let request = hearth_proto::RunRequest {
        mode: None,
        prompt: "another request".into(),
        harness: None,
        model: None,
        reasoning: None,
        model_options: serde_json::Map::new(),
        cwd: worktree.path.clone(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        attachments: Vec::new(),
        worktree: None,
        resume: None,
    };
    core.sessions
        .dispatch(chat_id, HarnessId::Mock, request, None)
        .await
        .expect("second dispatch");
    tokio::time::sleep(Duration::from_millis(400)).await;
    let chat = core.workspace.chat(chat_id).expect("chat").expect("row");
    assert_eq!(chat.title.as_deref(), Some("My Custom Name"));
    core.shutdown().await;
}

#[tokio::test]
async fn rename_worktree_branch_guards_and_collisions() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path().join("repo");
    init_repo(&repo_dir).await;
    let repos = Repos::with_worktrees_root(
        &tmp.path().join("data"),
        "device-test",
        tmp.path().join("worktrees"),
    );
    let wt = repos
        .create_worktree(&repo_dir, "main")
        .await
        .expect("worktree");
    let wt_path = Path::new(&wt.path);

    // Guard: expected branch mismatch → no-op, returns the actual branch.
    let unchanged = repos
        .rename_worktree_branch(wt_path, "hearth/not-this-one", "Some Title")
        .await
        .expect("guarded");
    assert_eq!(unchanged, wt.branch);

    // Happy path: renamed to the title slug.
    let renamed = repos
        .rename_worktree_branch(wt_path, &wt.branch, "Add Dark Mode!")
        .await
        .expect("renamed");
    assert_eq!(renamed, "hearth/add-dark-mode");

    // Already renamed → the guard (branch no longer hearth/<folder>) makes any
    // further title rename a no-op.
    let again = repos
        .rename_worktree_branch(wt_path, "hearth/add-dark-mode", "Different Title")
        .await
        .expect("second rename");
    assert_eq!(again, "hearth/add-dark-mode");

    // Collision: a second worktree whose title slug already exists gets the
    // stable hash suffix.
    let wt2 = repos
        .create_worktree(&repo_dir, "main")
        .await
        .expect("worktree 2");
    let renamed2 = repos
        .rename_worktree_branch(Path::new(&wt2.path), &wt2.branch, "Add Dark Mode!")
        .await
        .expect("suffixed rename");
    assert!(
        renamed2.starts_with("hearth/add-dark-mode-")
            && renamed2.len() == "hearth/add-dark-mode-".len() + 6,
        "suffixed: {renamed2}"
    );

    // Slug edge cases.
    assert_eq!(
        worktree_branch_from_title("  Fix `Login` Flow!  "),
        "hearth/fix-login-flow"
    );
    assert_eq!(worktree_branch_from_title("***"), "hearth/update");
    assert_eq!(
        worktree_branch_from_title("Cafe's Dark Mode"),
        "hearth/cafes-dark-mode"
    );
}

// ---------------------------------------------------------------------------
// RPC dispatch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rpc_dispatch_for_m5c_methods() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let core = assemble_with_mock(&tmp.path().join("data"), Vec::new());
    let client = hearth_rpc::memory_client(core.rpc_service());

    // Uploads: chunk → commit → readback over the wire.
    let payload = b"fake png bytes".to_vec();
    let ok = client
        .call(
            methods::UPLOAD_CHUNK,
            serde_json::json!({ "uploadId": "rpc-up", "data": BASE64.encode(&payload), "seq": 0 }),
        )
        .await
        .expect("UploadChunk");
    assert_eq!(ok["ok"], true);
    let committed = client
        .call(
            methods::UPLOAD_COMMIT,
            serde_json::json!({ "uploadId": "rpc-up", "fileName": "shot.png" }),
        )
        .await
        .expect("UploadCommit");
    let path = committed["path"].as_str().expect("path").to_string();
    assert!(path.ends_with("rpc-up-shot.png"));
    let chunk = client
        .call(
            methods::READ_ATTACHMENT_CHUNK,
            serde_json::json!({ "path": path, "offset": 0 }),
        )
        .await
        .expect("ReadAttachmentChunk");
    assert_eq!(chunk["mimeType"], "image/png");
    assert_eq!(chunk["done"], true);
    assert_eq!(
        BASE64
            .decode(chunk["data"].as_str().expect("data"))
            .expect("base64"),
        payload
    );
    // Jail holds over RPC too.
    assert!(
        client
            .call(
                methods::READ_ATTACHMENT_CHUNK,
                serde_json::json!({ "path": "/etc/passwd", "offset": 0 })
            )
            .await
            .is_err()
    );

    // Agent accounts: snapshot shape (this machine's real CLI state may or may
    // not include logins — assert the envelope, not the contents).
    let snapshot = client
        .call(methods::LIST_AGENT_ACCOUNTS, serde_json::json!({}))
        .await
        .expect("ListAgentAccounts");
    assert!(snapshot["accounts"].is_array());
    assert!(snapshot["warnings"].is_array());

    // Non-Codex logins fail (retired harness ids deserialize as Raven).
    assert!(
        client
            .call(
                methods::START_AGENT_LOGIN,
                serde_json::json!({ "harness": "raven" }),
            )
            .await
            .is_err()
    );
    assert!(
        client
            .call(
                methods::START_AGENT_LOGIN,
                serde_json::json!({ "harness": "claude-code" }),
            )
            .await
            .is_err()
    );

    // Error paths: junk account ids and dead logins fail cleanly.
    assert!(
        client
            .call(
                methods::FORGET_AGENT_ACCOUNT,
                serde_json::json!({ "harness": "codex", "accountId": "../nope" })
            )
            .await
            .is_err()
    );
    assert!(
        client
            .call(
                methods::ACTIVATE_AGENT_ACCOUNT,
                serde_json::json!({ "harness": "codex", "accountId": "0123456789abcdef" })
            )
            .await
            .is_err(),
        "unknown slot cannot be activated"
    );
    assert!(
        client
            .call(
                methods::COMPLETE_AGENT_LOGIN,
                serde_json::json!({ "loginId": "no-such-login", "code": "x#y" })
            )
            .await
            .is_err()
    );
    core.shutdown().await;
}
