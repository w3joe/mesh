use mesh_server::{build_router, AppState, MeshEngine};
use std::fs;
use std::net::SocketAddr;
use std::process::Command;
use std::sync::Arc;
use tempfile::TempDir;

const GIT_BIN: &str = "/opt/homebrew/bin/git";

fn run_git(cwd: &std::path::Path, args: &[&str]) -> String {
    let output = Command::new(GIT_BIN)
        .current_dir(cwd)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "Codex Agent")
        .env("GIT_AUTHOR_EMAIL", "codex@openai.com")
        .env("GIT_COMMITTER_NAME", "Codex Agent")
        .env("GIT_COMMITTER_EMAIL", "codex@openai.com")
        .output()
        .expect("failed to execute git binary");

    if !output.status.success() {
        panic!(
            "git {:?} failed in {:?}:\nstdout: {}\nstderr: {}",
            args,
            cwd,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    String::from_utf8_lossy(&output.stdout).to_string()
}

/// End-to-End Conformance Test using the real `/opt/homebrew/bin/git` CLI against `mesh-server`:
/// 1. `git push` initial commit to `refs/heads/main`.
/// 2. Two independent agent checkouts (`agent_a` and `agent_b`) `git clone` from `mesh-server`.
/// 3. `agent_a` and `agent_b` both edit the exact same file `src/api.rs` (adding imports at the
///    top and functions at the bottom) based on the same `main` commit.
/// 4. `agent_a` pushes to `main`.
/// 5. `agent_b` (without pulling or rebasing!) pushes to `HEAD:refs/mesh/land/main`.
/// 6. `mesh-server` performs server-side CST-commutative splicing and advances `refs/heads/main`.
/// 7. `agent_a` runs `git pull origin main` and verifies both agents' functions and imports are present!
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_real_git_cli_clone_push_and_commutative_ast_land() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");

    let state = AppState {
        engine: Arc::new(MeshEngine::new()),
        public_base_url: base_url.clone(),
    };
    let app = build_router(state.clone());

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let repo_url = format!("{base_url}/git/monorepo");
    let tmp = TempDir::new().unwrap();

    // 1. Seed initial repository and push `main`
    let seed_dir = tmp.path().join("seed");
    fs::create_dir_all(seed_dir.join("src")).unwrap();
    run_git(&seed_dir, &["init", "-b", "main"]);
    fs::write(
        seed_dir.join("src/api.rs"),
        "use std::collections::HashMap;\n\n/// Base route\npub fn health() -> &'static str {\n    \"ok\"\n}\n",
    )
    .unwrap();
    run_git(&seed_dir, &["add", "."]);
    run_git(&seed_dir, &["commit", "-m", "Initial seed commit"]);
    run_git(&seed_dir, &["push", &repo_url, "main"]);

    // 2. Clone into two parallel agent sandboxes using real `git clone`
    let agent_a_dir = tmp.path().join("agent_a");
    let agent_b_dir = tmp.path().join("agent_b");
    run_git(tmp.path(), &["clone", &repo_url, "agent_a"]);
    run_git(tmp.path(), &["clone", &repo_url, "agent_b"]);

    // 3. Agent A adds `use serde::Serialize;` at the top and `pub fn create_session()` at the bottom
    fs::write(
        agent_a_dir.join("src/api.rs"),
        "use std::collections::HashMap;\nuse serde::Serialize;\n\n/// Base route\npub fn health() -> &'static str {\n    \"ok\"\n}\n\n/// Added by Agent A\npub fn create_session(user: &str) -> String {\n    format!(\"session:{user}\")\n}\n",
    )
    .unwrap();
    run_git(&agent_a_dir, &["add", "."]);
    run_git(&agent_a_dir, &["commit", "-m", "Agent A: add create_session"]);
    run_git(&agent_a_dir, &["push", "origin", "main"]);

    // 4. Agent B (still on the OLD main commit!) edits the exact same `src/api.rs` lines,
    //    adding `use sha2::Sha256;` at the top and `pub fn verify_signature()` at the bottom.
    //    Instead of rebasing, Agent B pushes to `HEAD:refs/mesh/land/main`!
    fs::write(
        agent_b_dir.join("src/api.rs"),
        "use std::collections::HashMap;\nuse sha2::Sha256;\n\n/// Base route\npub fn health() -> &'static str {\n    \"ok\"\n}\n\n/// Added by Agent B\npub fn verify_signature(sig: &str) -> bool {\n    !sig.is_empty()\n}\n",
    )
    .unwrap();
    run_git(&agent_b_dir, &["add", "."]);
    run_git(
        &agent_b_dir,
        &["commit", "-m", "Agent B: add verify_signature"],
    );
    run_git(
        &agent_b_dir,
        &["push", "origin", "HEAD:refs/mesh/land/main"],
    );

    // 5. Agent A fetches/pulls the new `main` from `mesh-server` using real `git fetch` + `git reset --hard`
    run_git(&agent_a_dir, &["fetch", "origin", "main"]);
    run_git(&agent_a_dir, &["reset", "--hard", "origin/main"]);

    let merged_content = fs::read_to_string(agent_a_dir.join("src/api.rs")).unwrap();
    assert!(merged_content.contains("use serde::Serialize;"));
    assert!(merged_content.contains("use sha2::Sha256;"));
    assert!(merged_content.contains("pub fn create_session(user: &str)"));
    assert!(merged_content.contains("pub fn verify_signature(sig: &str)"));
}

/// End-to-End Test for the Cross-File Semantic Conflict Radar & Stats HTTP API:
/// Verifies that when two agents push topic branches with a cross-file signature/call-site
/// mismatch, `GET /api/v1/repos/:repo/radar` detects `SignatureCallMismatch` before merge.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_semantic_conflict_radar_and_stats_api() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");

    let state = AppState {
        engine: Arc::new(MeshEngine::new()),
        public_base_url: base_url.clone(),
    };
    let app = build_router(state);

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let repo_url = format!("{base_url}/git/monorepo");
    let tmp = TempDir::new().unwrap();

    // 1. Seed initial repo with `src/billing.rs` and `src/checkout.rs`
    let seed = tmp.path().join("seed");
    fs::create_dir_all(seed.join("src")).unwrap();
    run_git(&seed, &["init", "-b", "main"]);
    fs::write(
        seed.join("src/billing.rs"),
        "pub fn charge(amount: u64) -> bool {\n    amount > 0\n}\n",
    )
    .unwrap();
    fs::write(
        seed.join("src/checkout.rs"),
        "use crate::billing::charge;\n\npub fn ready() -> bool {\n    true\n}\n",
    )
    .unwrap();
    run_git(&seed, &["add", "."]);
    run_git(&seed, &["commit", "-m", "Seed billing and checkout"]);
    run_git(&seed, &["push", &repo_url, "main"]);

    // 2. Agent 1 changes signature of `charge` on branch `agent-billing`
    let w1 = tmp.path().join("w1");
    run_git(tmp.path(), &["clone", &repo_url, "w1"]);
    run_git(&w1, &["checkout", "-b", "agent-billing"]);
    fs::write(
        w1.join("src/billing.rs"),
        "pub fn charge(amount: u64, currency: &str) -> bool {\n    amount > 0 && !currency.is_empty()\n}\n",
    )
    .unwrap();
    run_git(&w1, &["add", "."]);
    run_git(&w1, &["commit", "-m", "Change charge signature"]);
    run_git(&w1, &["push", "origin", "agent-billing"]);

    // 3. Agent 2 adds a call to `charge(100)` in `src/checkout.rs` on branch `agent-checkout`
    let w2 = tmp.path().join("w2");
    run_git(tmp.path(), &["clone", &repo_url, "w2"]);
    run_git(&w2, &["checkout", "-b", "agent-checkout"]);
    fs::write(
        w2.join("src/checkout.rs"),
        "use crate::billing::charge;\n\npub fn ready() -> bool {\n    true\n}\n\npub fn process_order() -> bool {\n    charge(100)\n}\n",
    )
    .unwrap();
    run_git(&w2, &["add", "."]);
    run_git(&w2, &["commit", "-m", "Call charge(100) in process_order"]);
    run_git(&w2, &["push", "origin", "agent-checkout"]);

    // 4. Query the live Semantic Conflict Radar HTTP endpoint
    let client = reqwest::Client::new();
    let conflicts: Vec<mesh_proto::SemanticConflict> = client
        .get(format!("{base_url}/api/v1/repos/monorepo/radar"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(conflicts.len(), 1);
    assert_eq!(
        conflicts[0].kind,
        mesh_proto::SemanticConflictKind::SignatureCallMismatch
    );
    assert_eq!(conflicts[0].symbol_name, "charge");

    // 5. Query repository stats endpoint
    let stats: mesh_server::RepoStatsResponse = client
        .get(format!("{base_url}/api/v1/repos/monorepo/stats"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(stats.total_refs, 3); // main + agent-billing + agent-checkout
    assert_eq!(stats.advertised_refs_on_push, 1); // Pando trunk-only advertisement!
    assert!(stats.push_advertisement_bytes < 256);
    assert_eq!(stats.active_semantic_conflicts, 1);
}
