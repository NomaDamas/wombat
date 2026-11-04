use assert_cmd::Command;
use serde_json::{Value, json};
use tempfile::TempDir;

fn minsync_available() -> bool {
    std::process::Command::new("minsync")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn sync_json(args: &[String], agent: &str) -> Value {
    let out = assert_cmd::cargo::cargo_bin_cmd!("wombat")
        .args(args)
        .args(["sync", "--agent", agent, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&out).unwrap()
}

#[test]
fn agent_scoped_sync_keeps_other_agents_skipped_items() {
    if !minsync_available() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let codex = tmp.path().join("codex");
    let pi = tmp.path().join("pi/--Volumes-x-proj--");
    std::fs::create_dir_all(&codex).unwrap();
    std::fs::create_dir_all(&pi).unwrap();

    // Codex headless exec session -> always skipped, never in the catalog.
    let lines = [
        json!({"type": "session_meta", "payload": {"session_id": "exec-1", "cwd": "/Users/a/proj", "originator": "codex_exec"}}),
        json!({"type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "review PR"}]}}),
    ];
    std::fs::write(
        codex.join("rollout-exec.jsonl"),
        lines.iter().map(|l| format!("{l}\n")).collect::<String>(),
    )
    .unwrap();
    let pi_lines = [
        json!({"type": "session", "id": "pi-1", "cwd": "/Users/a/proj"}),
        json!({"type": "message", "message": {"role": "user", "content": [{"type": "text", "text": "hello pi"}]}}),
    ];
    std::fs::write(
        pi.join("2026-11-04_pi-1.jsonl"),
        pi_lines
            .iter()
            .map(|l| format!("{l}\n"))
            .collect::<String>(),
    )
    .unwrap();

    let args: Vec<String> = [
        "--data-dir".to_string(),
        tmp.path().join("data").display().to_string(),
        "--embedder".into(),
        "none".into(),
        "--source".into(),
        format!("codex={}", codex.display()),
        "--source".into(),
        format!("pi={}", tmp.path().join("pi").display()),
    ]
    .to_vec();

    let first = sync_json(&args, "codex");
    assert_eq!(first["processed"], 1);
    assert_eq!(first["skipped"], 1);

    // A sync scoped to another agent must not forget codex's skipped item.
    let pi_run = sync_json(&args, "pi");
    assert_eq!(pi_run["deleted"], 0, "{pi_run}");

    // So the next codex sync has nothing to re-parse.
    let again = sync_json(&args, "codex");
    assert_eq!(again["processed"], 0, "{again}");
    let _ = Command::new("true");
}
