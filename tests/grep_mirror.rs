use assert_cmd::Command;
use serde_json::{Value, json};
use tempfile::TempDir;

fn wombat_cmd() -> Command {
    assert_cmd::cargo::cargo_bin_cmd!("wombat")
}

fn minsync_available() -> bool {
    std::process::Command::new("minsync")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn setup_synced_sessions() -> (TempDir, std::path::PathBuf) {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("wombat-data");
    let sources_dir = tmp.path().join("sources");
    let claude_dir = sources_dir.join("claude/proj-alpha");
    let pi_dir = sources_dir.join("pi/proj-beta");
    std::fs::create_dir_all(&claude_dir).unwrap();
    std::fs::create_dir_all(&pi_dir).unwrap();

    std::fs::write(
        claude_dir.join("sess-alpha.jsonl"),
        format!(
            "{}\n{}\n{}\n{}\n",
            json!({
                "type": "user",
                "sessionId": "sess-alpha",
                "cwd": "/work/proj-alpha",
                "timestamp": "2026-10-16T01:00:00Z",
                "message": {"role": "user", "content": "Investigate ERR_4092_QUASAR in the sync daemon."}
            }),
            json!({
                "type": "assistant",
                "sessionId": "sess-alpha",
                "timestamp": "2026-10-16T01:00:10Z",
                "message": {"role": "assistant", "content": "Line one context.\nThe code ERR_4092_QUASAR comes from stale lock files.\nLine three context."}
            }),
            json!({
                "type": "user",
                "sessionId": "sess-alpha",
                "timestamp": "2026-10-16T01:01:00Z",
                "message": {"role": "user", "content": "Also check ticket ABC-987654 for regression."}
            }),
            json!({
                "type": "assistant",
                "sessionId": "sess-alpha",
                "timestamp": "2026-10-16T01:01:10Z",
                "message": {"role": "assistant", "content": "Confirmed ABC-987654 was fixed in commit f91a2bc."}
            })
        ),
    )
    .unwrap();

    std::fs::write(
        pi_dir.join("sess-beta.jsonl"),
        format!(
            "{}\n{}\n{}\n",
            json!({
                "type": "session",
                "id": "sess-beta",
                "cwd": "/work/proj-beta",
                "timestamp": "2026-10-17T02:00:00Z"
            }),
            json!({
                "type": "message",
                "id": "m1",
                "timestamp": "2026-10-17T02:00:05Z",
                "message": {"role": "user", "content": [{"type": "text", "text": "Does proj-beta hit err_4092_quasar too?"}]}
            }),
            json!({
                "type": "message",
                "id": "m2",
                "timestamp": "2026-10-17T02:00:15Z",
                "message": {"role": "assistant", "content": [{"type": "text", "text": "No, proj-beta uses token BETA_TOKEN_7788 instead."}]}
            })
        ),
    )
    .unwrap();

    wombat_cmd()
        .args([
            "--data-dir",
            data_dir.to_str().unwrap(),
            "--source",
            &format!("claude-code={}", claude_dir.display()),
            "--source",
            &format!("pi={}", pi_dir.display()),
            "--embedder",
            "none",
            "--json",
            "sync",
        ])
        .assert()
        .success();

    (tmp, data_dir)
}

#[test]
fn grep_exact_literal_and_ignore_case_with_turn_mapping_and_context() {
    if !minsync_available() {
        return;
    }
    let (_tmp, data_dir) = setup_synced_sessions();

    // Case-sensitive literal match finds only claude-code session.
    let out = wombat_cmd()
        .args([
            "--data-dir",
            data_dir.to_str().unwrap(),
            "--json",
            "grep",
            "--literal",
            "--context",
            "1",
            "ERR_4092_QUASAR",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let hits: Vec<Value> = serde_json::from_slice(&out).unwrap();
    assert_eq!(
        hits.len(),
        2,
        "expected 2 matching lines in sess-alpha: {hits:?}"
    );
    assert!(hits.iter().all(|h| h["agent"] == "claude-code"));
    assert!(
        hits.iter()
            .all(|h| h["catalog_key"] == "claude-code:sess-alpha")
    );
    assert_eq!(hits[0]["turn_index"], 1);
    assert_eq!(hits[1]["turn_index"], 2);
    assert!(
        hits[1]["snippet"]
            .as_str()
            .unwrap()
            .contains("Line one context.")
            && hits[1]["snippet"]
                .as_str()
                .unwrap()
                .contains("Line three context."),
        "context=1 should include surrounding mirror lines: {:?}",
        hits[1]["snippet"]
    );
    assert!(hits[0]["line_number"].as_u64().unwrap() >= 1);

    // Case-insensitive match finds both claude-code and pi sessions.
    let out_ci = wombat_cmd()
        .args([
            "--data-dir",
            data_dir.to_str().unwrap(),
            "--json",
            "grep",
            "--literal",
            "--ignore-case",
            "ERR_4092_QUASAR",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let hits_ci: Vec<Value> = serde_json::from_slice(&out_ci).unwrap();
    assert_eq!(
        hits_ci.len(),
        3,
        "expected hits across both sessions: {hits_ci:?}"
    );
    let agents: Vec<&str> = hits_ci
        .iter()
        .map(|h| h["agent"].as_str().unwrap())
        .collect();
    assert!(agents.contains(&"claude-code"));
    assert!(agents.contains(&"pi"));
}

#[test]
fn grep_filters_by_agent_project_regex_and_limit() {
    if !minsync_available() {
        return;
    }
    let (_tmp, data_dir) = setup_synced_sessions();

    // Regex match across both sessions.
    let out = wombat_cmd()
        .args([
            "--data-dir",
            data_dir.to_str().unwrap(),
            "--json",
            "grep",
            "(ABC-\\d{6}|BETA_TOKEN_\\d{4})",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let hits: Vec<Value> = serde_json::from_slice(&out).unwrap();
    assert_eq!(hits.len(), 3, "expected 2 ABC matches + 1 BETA_TOKEN match");

    // Agent filter restricts to pi only.
    let out_pi = wombat_cmd()
        .args([
            "--data-dir",
            data_dir.to_str().unwrap(),
            "--json",
            "grep",
            "--agent",
            "pi",
            "(ABC-\\d{6}|BETA_TOKEN_\\d{4})",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let hits_pi: Vec<Value> = serde_json::from_slice(&out_pi).unwrap();
    assert_eq!(hits_pi.len(), 1);
    assert_eq!(hits_pi[0]["agent"], "pi");
    assert_eq!(hits_pi[0]["turn_index"], 2);

    // Project filter restricts to proj-alpha.
    let out_proj = wombat_cmd()
        .args([
            "--data-dir",
            data_dir.to_str().unwrap(),
            "--json",
            "grep",
            "--project",
            "proj-alpha",
            "--limit",
            "1",
            "ABC-987654",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let hits_proj: Vec<Value> = serde_json::from_slice(&out_proj).unwrap();
    assert_eq!(hits_proj.len(), 1);
    assert_eq!(hits_proj[0]["catalog_key"], "claude-code:sess-alpha");
}

#[test]
fn grep_skips_minsync_internal_directory_and_works_without_rg_on_path() {
    if !minsync_available() {
        return;
    }
    let (_tmp, data_dir) = setup_synced_sessions();

    // Plant a file inside mirror/.minsync containing a sentinel; grep must never return it.
    let minsync_dir = data_dir.join("mirror").join(".minsync");
    std::fs::create_dir_all(&minsync_dir).unwrap();
    std::fs::write(
        minsync_dir.join("internal.md"),
        "INTERNAL_MINSYNC_ONLY_SENTINEL\n",
    )
    .unwrap();

    let out_internal = wombat_cmd()
        .args([
            "--data-dir",
            data_dir.to_str().unwrap(),
            "--json",
            "grep",
            "--literal",
            "INTERNAL_MINSYNC_ONLY_SENTINEL",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let internal_hits: Vec<Value> = serde_json::from_slice(&out_internal).unwrap();
    assert!(internal_hits.is_empty(), "must never grep mirror/.minsync");

    // Verify pure-Rust fallback when PATH has no `rg`.
    let out_no_rg = wombat_cmd()
        .env("PATH", "")
        .args([
            "--data-dir",
            data_dir.to_str().unwrap(),
            "--json",
            "grep",
            "--literal",
            "BETA_TOKEN_7788",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let fallback_hits: Vec<Value> = serde_json::from_slice(&out_no_rg).unwrap();
    assert_eq!(fallback_hits.len(), 1);
    assert_eq!(fallback_hits[0]["catalog_key"], "pi:sess-beta");
}
