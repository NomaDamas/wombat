use rusqlite::{Connection, params};
use serde_json::json;
use std::io::Write;
use std::path::Path;
use tempfile::TempDir;
use wombat::mirror::render_mirror;
use wombat::model::{AgentKind, Role, SourceOffset};
use wombat::parsers::{
    ParseOptions, ParseOutcome, SourceRoots, discover_all, extract_raw_trace, parse_candidate,
};

#[test]
fn pi_skips_autorag_harness_sessions_even_in_real_project_dirs() {
    let dir = TempDir::new().unwrap();
    let proj = dir.path().join("--Volumes-SSD1-4-AutoRAG--");
    std::fs::create_dir_all(&proj).unwrap();
    let mk = |name: &str, first: &str| {
        write_jsonl_lines(
            &proj.join(name),
            &[
                json!({"type": "session", "id": name, "cwd": "/Volumes/SSD1/4/AutoRAG"}),
                json!({"type": "message", "message": {"role": "user", "content": [{"type": "text", "text": first}]}}),
                json!({"type": "message", "message": {"role": "assistant", "content": [{"type": "text", "text": "ok"}]}}),
            ],
        );
    };
    mk(
        "a_harness.jsonl",
        "Answer this original query immediately: Who approves refunds?\n\nBaseline retrieval evidence",
    );
    mk("b_human.jsonl", "리팩터링 계획 먼저 세워줘");
    let opts = ParseOptions::default();
    let cands = wombat::parsers::pi::discover(dir.path(), AgentKind::Pi, &opts).unwrap();
    let kept: Vec<_> = cands
        .iter()
        .filter_map(|c| match parse_candidate(c, &opts).unwrap() {
            ParseOutcome::Session(s) => Some(s.session_id),
            ParseOutcome::Skipped { .. } => None,
        })
        .collect();
    assert_eq!(kept, vec!["b_human.jsonl".to_string()]);
}
#[test]
fn automation_workspaces_are_ephemeral_but_real_projects_are_not() {
    use wombat::model::is_ephemeral_temp_cwd;
    for c in [
        "/tmp/x",
        "/private/var/folders/ab/T/run",
        "/Users/alice/.autorag/workspace",
        "/Users/alice/.autorag/workspace/sub",
        "/Volumes/SSD1/4/AutoRAG/.bench/ws-noemit",
        "/Volumes/SSD1/4/AutoRAG/.bench-ws-main",
    ] {
        assert!(is_ephemeral_temp_cwd(c), "{c} should be ephemeral");
    }
    for c in [
        "/Volumes/SSD1/1/wombat",
        "/Users/alice/.autorag-notes",
        "/Users/alice/Projects/autorag/workspace",
        "/Users/alice/Projects/workbench/notes",
    ] {
        assert!(!is_ephemeral_temp_cwd(c), "{c} should be kept");
    }
}

fn write_zst_lines(path: &Path, lines: &[serde_json::Value]) {
    let file = std::fs::File::create(path).unwrap();
    let mut enc = zstd::stream::write::Encoder::new(file, 3).unwrap();
    for line in lines {
        let s = serde_json::to_string(line).unwrap();
        enc.write_all(s.as_bytes()).unwrap();
        enc.write_all(b"\n").unwrap();
    }
    enc.finish().unwrap();
}

fn write_jsonl_lines(path: &Path, lines: &[serde_json::Value]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut out = String::new();
    for l in lines {
        out.push_str(&serde_json::to_string(l).unwrap());
        out.push('\n');
    }
    std::fs::write(path, out).unwrap();
}

#[test]
fn codex_zst_parser_strips_system_reasoning_and_tools_and_keeps_line_offsets() {
    let dir = TempDir::new().unwrap();
    let zst_path = dir
        .path()
        .join("rollout-2026-10-15T21-58-01-01a13fa4-1b1a-7c03-9fcd-cce611b321ca.jsonl.zst");

    let lines = vec![
        json!({
            "timestamp": "2026-10-15T12:58:01.885Z",
            "type": "session_meta",
            "payload": {
                "session_id": "01a13fa4-1b1a-7c03-9fcd-cce611b321ca",
                "timestamp": "2026-10-15T12:58:01.885Z",
                "cwd": "/Users/alice/Projects/banana",
                "thread_source": "user"
            }
        }),
        json!({
            "timestamp": "2026-10-15T12:58:20.512Z",
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "developer",
                "content": [{"type": "input_text", "text": "<app-context>SECRET_DEVELOPER_PROMPT</app-context>"}]
            }
        }),
        json!({
            "timestamp": "2026-10-15T12:58:20.512Z",
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "# AGENTS.md instructions\n\n<INSTRUCTIONS>NOISE_AGENTS_MD</INSTRUCTIONS>"},
                    {"type": "input_text", "text": "<environment_context><cwd>/Users/alice/Projects/banana</cwd></environment_context>"}
                ]
            }
        }),
        json!({
            "timestamp": "2026-10-15T12:58:20.517Z",
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "1Password Chrome CSV 마이그레이션 진행해줘"}]
            }
        }),
        json!({
            "timestamp": "2026-10-15T12:58:30.000Z",
            "type": "response_item",
            "payload": {
                "type": "reasoning",
                "summary": [{"type": "summary_text", "text": "SECRET_THINKING_TRACE"}]
            }
        }),
        json!({
            "timestamp": "2026-10-15T12:58:35.000Z",
            "type": "response_item",
            "payload": {
                "type": "custom_tool_call",
                "name": "exec_command",
                "input": "op account list --SECRET_TOOL_CALL"
            }
        }),
        json!({
            "timestamp": "2026-10-15T12:58:36.000Z",
            "type": "response_item",
            "payload": {
                "type": "custom_tool_call_output",
                "output": "SECRET_TOOL_OUTPUT_DUMP_DIFF_PATCH"
            }
        }),
        json!({
            "timestamp": "2026-10-15T12:59:14.192Z",
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "assistant",
                "phase": "final_answer",
                "content": [{"type": "output_text", "text": "Chrome CSV 514개 항목을 순차 등록하겠습니다."}]
            }
        }),
    ];
    write_zst_lines(&zst_path, &lines);

    let roots = SourceRoots {
        codex: vec![dir.path().to_path_buf()],
        ..Default::default()
    };
    let opts = ParseOptions::default();
    let candidates = discover_all(&roots, None, &opts).unwrap();
    assert_eq!(candidates.len(), 1);

    let ParseOutcome::Session(session) = parse_candidate(&candidates[0], &opts).unwrap() else {
        panic!("expected parsed codex session");
    };
    assert_eq!(session.agent, AgentKind::Codex);
    assert_eq!(session.session_id, "01a13fa4-1b1a-7c03-9fcd-cce611b321ca");
    assert_eq!(session.turns.len(), 2);
    assert_eq!(session.turns[0].role, Role::User);
    assert_eq!(
        session.turns[0].text,
        "1Password Chrome CSV 마이그레이션 진행해줘"
    );
    assert_eq!(
        session.turns[0].source_offset,
        SourceOffset::Lines {
            start_line: 4,
            end_line: 4
        }
    );
    assert_eq!(session.turns[1].role, Role::Assistant);
    assert_eq!(
        session.turns[1].source_offset,
        SourceOffset::Lines {
            start_line: 8,
            end_line: 8
        }
    );

    let mirror = render_mirror(&session);
    for forbidden in [
        "SECRET_DEVELOPER_PROMPT",
        "NOISE_AGENTS_MD",
        "SECRET_THINKING_TRACE",
        "SECRET_TOOL_CALL",
        "SECRET_TOOL_OUTPUT_DUMP",
    ] {
        assert!(
            !mirror.content.contains(forbidden),
            "mirror leaked {forbidden}"
        );
    }

    // Verify full trace extraction from zstd file!
    let raw_trace = extract_raw_trace(
        AgentKind::Codex,
        &session.source_path,
        &session.turns[1].source_offset,
        2,
    )
    .unwrap();
    assert!(raw_trace.contains("SECRET_TOOL_OUTPUT_DUMP_DIFF_PATCH"));
    assert!(raw_trace.contains("Chrome CSV 514개"));

    // Verify Codex subagent skipped by default.
    let sub_path = dir.path().join("rollout-subagent.jsonl");
    write_jsonl_lines(
        &sub_path,
        &[
            json!({
                "type": "session_meta",
                "payload": {
                    "session_id": "sub-1",
                    "cwd": "/Users/alice/Projects/banana",
                    "thread_source": {"subagent": {"thread_spawn": {"parent_thread_id": "parent-1"}}}
                }
            }),
            json!({
                "type": "response_item",
                "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "subtask"}]}
            }),
        ],
    );
    let sub_cand = wombat::parsers::codex::discover(dir.path())
        .unwrap()
        .into_iter()
        .find(|c| c.fs_path == sub_path)
        .unwrap();
    assert!(matches!(
        parse_candidate(&sub_cand, &opts).unwrap(),
        ParseOutcome::Skipped { reason: "subagent" }
    ));
}

#[test]
fn codex_headless_exec_sessions_are_skipped_unless_ephemeral_included() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("rollout-exec.jsonl");
    write_jsonl_lines(
        &path,
        &[
            json!({"type": "session_meta", "payload": {
                "session_id": "exec-1", "cwd": "/Users/alice/Projects/k-skill",
                "originator": "codex_exec", "thread_source": "user"}}),
            json!({"type": "response_item", "payload": {"type": "message", "role": "user",
                "content": [{"type": "input_text", "text": "You are reviewing PR #405 in NomaDamas/k-skill"}]}}),
            json!({"type": "response_item", "payload": {"type": "message", "role": "assistant",
                "content": [{"type": "output_text", "text": "verdict: ok"}]}}),
        ],
    );
    let cand = wombat::parsers::codex::discover(dir.path())
        .unwrap()
        .remove(0);
    assert!(matches!(
        parse_candidate(&cand, &ParseOptions::default()).unwrap(),
        ParseOutcome::Skipped {
            reason: "headless_exec"
        }
    ));
    let keep = ParseOptions {
        include_ephemeral: true,
        ..Default::default()
    };
    assert!(matches!(
        parse_candidate(&cand, &keep).unwrap(),
        ParseOutcome::Session(_)
    ));
}

#[test]
fn claude_code_parser_strips_commands_thinking_tools_and_snapshots() {
    let dir = TempDir::new().unwrap();
    let proj = dir.path().join("-Volumes-SSD1-1-wombat");
    let sub_dir = proj.join("sess-1").join("subagents");
    std::fs::create_dir_all(&sub_dir).unwrap();

    // Subagent file in `subagents/` must be ignored during discovery.
    write_jsonl_lines(
        &sub_dir.join("agent-sub.jsonl"),
        &[json!({
            "type": "user",
            "sessionId": "sub-1",
            "message": {"role": "user", "content": "subagent task"}
        })],
    );

    let main_file = proj.join("e46ba661-b6eb-4ad4-acb6-1e8b56a0d143.jsonl");
    write_jsonl_lines(
        &main_file,
        &[
            json!({
                "type": "user",
                "isMeta": true,
                "sessionId": "e46ba661-b6eb-4ad4-acb6-1e8b56a0d143",
                "cwd": "/Volumes/SSD1/1/wombat",
                "timestamp": "2026-07-04T05:59:20.000Z",
                "message": {"role": "user", "content": "<local-command-caveat>NOISE_CAVEAT</local-command-caveat>"}
            }),
            json!({
                "type": "user",
                "sessionId": "e46ba661-b6eb-4ad4-acb6-1e8b56a0d143",
                "timestamp": "2026-07-04T05:59:21.000Z",
                "message": {"role": "user", "content": "<command-name>/login</command-name>"}
            }),
            json!({
                "type": "user",
                "sessionId": "e46ba661-b6eb-4ad4-acb6-1e8b56a0d143",
                "timestamp": "2026-07-04T05:59:29.158Z",
                "message": {"role": "user", "content": "마이크로서비스 통합 아키텍처를 정리해줘"}
            }),
            // Duplicate queued user echo at identical timestamp -> deduped
            json!({
                "type": "user",
                "sessionId": "e46ba661-b6eb-4ad4-acb6-1e8b56a0d143",
                "timestamp": "2026-07-04T05:59:29.158Z",
                "message": {"role": "user", "content": "마이크로서비스 통합 아키텍처를 정리해줘"}
            }),
            json!({
                "type": "file-history-snapshot",
                "snapshot": {"trackedFileBackups": {"src/main.rs": "DIFF_SNAPSHOT_NOISE"}}
            }),
            json!({
                "type": "assistant",
                "timestamp": "2026-07-04T06:00:00.000Z",
                "message": {
                    "role": "assistant",
                    "content": [
                        {"type": "thinking", "thinking": "CLAUDE_INTERNAL_THINKING"},
                        {"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "ls -la"}},
                        {"type": "text", "text": "총 7개 서비스의 API 경계를 먼저 정의했습니다."}
                    ]
                }
            }),
            json!({
                "type": "user",
                "timestamp": "2026-07-04T06:00:05.000Z",
                "message": {
                    "role": "user",
                    "content": [{"type": "tool_result", "tool_use_id": "toolu_1", "content": "CLAUDE_TOOL_RESULT_NOISE"}]
                }
            }),
            json!({
                "type": "ai-title",
                "aiTitle": "K-Manus 마이크로서비스 통합"
            }),
        ],
    );

    let opts = ParseOptions::default();
    let cands = wombat::parsers::claude_code::discover(dir.path(), &opts).unwrap();
    assert_eq!(cands.len(), 1);
    let ParseOutcome::Session(s) = parse_candidate(&cands[0], &opts).unwrap() else {
        panic!("expected claude session");
    };
    assert_eq!(s.title.as_deref(), Some("K-Manus 마이크로서비스 통합"));
    assert_eq!(s.turns.len(), 2);
    assert_eq!(s.turns[0].text, "마이크로서비스 통합 아키텍처를 정리해줘");
    assert_eq!(
        s.turns[1].text,
        "총 7개 서비스의 API 경계를 먼저 정의했습니다."
    );
    let rendered = render_mirror(&s);
    assert!(!rendered.content.contains("CLAUDE_INTERNAL_THINKING"));
    assert!(!rendered.content.contains("CLAUDE_TOOL_RESULT_NOISE"));
    assert!(!rendered.content.contains("DIFF_SNAPSHOT_NOISE"));
    assert!(!rendered.content.contains("K-Manus 마이크로서비스 통합"));
}

#[test]
fn pi_and_omp_parsers_filter_temp_faux_and_subagents_and_strip_reminders() {
    let dir = TempDir::new().unwrap();
    let real_proj = dir.path().join("--Volumes-SSD1-1-mailcrawl--");
    let tmp_proj = dir.path().join("--tmp-autorag-qa-ws--");
    let nested_sub = real_proj.join("2026-10-13T05-59-20-912Z_01a133d8");
    std::fs::create_dir_all(&nested_sub).unwrap();
    std::fs::create_dir_all(&tmp_proj).unwrap();

    // Ephemeral `/tmp` session -> skipped during discovery
    write_jsonl_lines(
        &tmp_proj.join("2026-10-13_tmp12345.jsonl"),
        &[
            json!({"type": "session", "id": "tmp12345", "cwd": "/tmp/autorag-qa-ws"}),
            json!({"type": "message", "message": {"role": "user", "content": [{"type": "text", "text": "ephemeral"}]}}),
        ],
    );

    // Nested subagent -> skipped by default
    write_jsonl_lines(
        &nested_sub.join("Pr42Review.jsonl"),
        &[
            json!({"type": "session", "id": "sub99999", "cwd": "/Volumes/SSD1/1/mailcrawl"}),
            json!({"type": "message", "message": {"role": "user", "content": [{"type": "text", "text": "review diff"}]}}),
        ],
    );

    // Faux provider automation in real dir -> skipped during parse
    write_jsonl_lines(
        &real_proj.join("2026-10-13_faux0001.jsonl"),
        &[
            json!({"type": "session", "id": "faux0001", "cwd": "/Volumes/SSD1/1/mailcrawl"}),
            json!({"type": "model_change", "provider": "faux", "modelId": "automations"}),
            json!({"type": "message", "message": {"role": "user", "content": [{"type": "text", "text": "faux prompt"}]}}),
        ],
    );

    // Real OMP user conversation with volatile title, developer rule violation, thinking, toolCall, toolResult
    write_jsonl_lines(
        &real_proj.join("2026-10-13T05-59-20-912Z_01a133d8.jsonl"),
        &[
            json!({"type": "title", "v": 1, "title": "Implement issue 41", "pad": "     "}),
            json!({"type": "session", "version": 3, "id": "01a133d8", "timestamp": "2026-10-13T05:59:20.912Z", "cwd": "/Volumes/SSD1/1/mailcrawl"}),
            json!({"type": "message", "timestamp": "2026-10-13T05:59:20.950Z", "message": {"role": "system", "content": "OMP_SYSTEM_PROMPT_SECRET"}}),
            json!({"type": "message", "timestamp": "2026-10-13T05:59:21.000Z", "message": {
                "role": "user",
                "attribution": "user",
                "content": [{"type": "text", "text": "<system-reminder>Today: 2026-10-13</system-reminder>한국어 BM25 토크나이저 교체를 진행해줘\n<attachment>\nDUMPED_CRASH_LOG_ATTACHMENT_SECRET\n</attachment>\n<system-notice>\nSUBAGENT_TASK_NOTICE_SECRET\n</system-notice>"}]
            }}),
            json!({"type": "message", "timestamp": "2026-10-13T05:59:25.000Z", "message": {
                "role": "developer",
                "attribution": "agent",
                "content": [{"type": "text", "text": "<system-reminder>OMP_RULE_VIOLATION</system-reminder>"}]
            }}),
            json!({"type": "message", "timestamp": "2026-10-13T05:59:30.000Z", "message": {
                "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": "OMP_THINKING_SECRET"},
                    {"type": "toolCall", "id": "call_1", "name": "edit", "arguments": {"input": "DIFF_HUNK_SECRET"}},
                    {"type": "text", "text": "Kiwi 형태소 분석기 기반으로 교체했습니다."}
                ]
            }}),
            json!({"type": "message", "timestamp": "2026-10-13T05:59:31.000Z", "message": {
                "role": "toolResult",
                "content": [{"type": "text", "text": "OMP_TOOL_RESULT_SECRET"}]
            }}),
        ],
    );

    let opts = ParseOptions::default();
    let cands = wombat::parsers::pi::discover(dir.path(), AgentKind::Omp, &opts).unwrap();
    assert_eq!(cands.len(), 2); // faux + real; tmp & nested subagent skipped at discovery

    let mut sessions = Vec::new();
    for c in &cands {
        if let ParseOutcome::Session(s) = parse_candidate(c, &opts).unwrap() {
            sessions.push(s);
        }
    }
    assert_eq!(sessions.len(), 1);
    let s = &sessions[0];
    assert_eq!(s.session_id, "01a133d8");
    assert_eq!(s.turns.len(), 2);
    assert_eq!(s.turns[0].text, "한국어 BM25 토크나이저 교체를 진행해줘");
    assert_eq!(s.turns[1].text, "Kiwi 형태소 분석기 기반으로 교체했습니다.");
    let mirror = render_mirror(s);
    for secret in [
        "OMP_SYSTEM_PROMPT_SECRET",
        "OMP_RULE_VIOLATION",
        "OMP_THINKING_SECRET",
        "DIFF_HUNK_SECRET",
        "OMP_TOOL_RESULT_SECRET",
        "Today: 2026-10-13",
        "DUMPED_CRASH_LOG_ATTACHMENT_SECRET",
        "SUBAGENT_TASK_NOTICE_SECRET",
    ] {
        assert!(!mirror.content.contains(secret), "leaked {secret}");
    }
}

#[test]
fn opencode_run_prompts_stored_as_json_strings_are_unquoted() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("opencode.db");
    {
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY, parent_id TEXT, directory TEXT, title TEXT, time_created INTEGER, time_updated INTEGER);
             CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);
             CREATE TABLE part (id TEXT PRIMARY KEY, session_id TEXT, message_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session VALUES ('ses_q', NULL, '/Users/a/proj', 'T', 1760000000000, 1760000001000)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message VALUES ('m_u', 'ses_q', 1760000001000, 1760000001000, ?1)",
            params![json!({"role": "user", "time": {"created": 1760000001000i64}}).to_string()],
        )
        .unwrap();
        // `opencode run "<prompt>"` stores the text part as a JSON-encoded string.
        conn.execute(
            "INSERT INTO part VALUES ('p1', 'ses_q', 'm_u', 1, 1, ?1)",
            params![
                json!({"type": "text", "text": "\"한 줄 질문입니다: 따옴표 \\\"안\\\" 보존\""})
                    .to_string()
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message VALUES ('m_a', 'ses_q', 1760000002000, 1760000002000, ?1)",
            params![
                json!({"role": "assistant", "time": {"created": 1760000002000i64}}).to_string()
            ],
        )
        .unwrap();
        // A genuine assistant answer that merely starts and ends with quotes must stay untouched.
        conn.execute(
            "INSERT INTO part VALUES ('p2', 'ses_q', 'm_a', 2, 2, ?1)",
            params![json!({"type": "text", "text": "\"quoted\" is a word, not JSON"}).to_string()],
        )
        .unwrap();
    }
    let opts = ParseOptions::default();
    let cand = wombat::parsers::opencode::discover(dir.path(), &opts)
        .unwrap()
        .remove(0);
    let ParseOutcome::Session(s) = parse_candidate(&cand, &opts).unwrap() else {
        panic!("expected session");
    };
    assert_eq!(s.turns[0].text, "한 줄 질문입니다: 따옴표 \"안\" 보존");
    assert_eq!(s.turns[1].text, "\"quoted\" is a word, not JSON");
}

#[test]
fn opencode_trace_of_a_reply_includes_the_tool_messages_of_its_turn() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("opencode.db");
    {
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY, parent_id TEXT, directory TEXT, title TEXT, time_created INTEGER, time_updated INTEGER);
             CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);
             CREATE TABLE part (id TEXT PRIMARY KEY, session_id TEXT, message_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session VALUES ('ses_t', NULL, '/Users/a/proj', 'T', 1760000000000, 1760000009000)",
            [],
        )
        .unwrap();
        // user -> assistant(tool edit) -> assistant(final text) -> user2 -> assistant(final text)
        let msgs = [
            ("m1", "user", None),
            ("m2", "assistant", Some("m1")),
            ("m3", "assistant", Some("m1")),
            ("m4", "user", None),
            ("m5", "assistant", Some("m4")),
        ];
        for (i, (id, role, parent)) in msgs.iter().enumerate() {
            let t = 1760000001000i64 + i as i64 * 1000;
            conn.execute(
                "INSERT INTO message VALUES (?1, 'ses_t', ?2, ?2, ?3)",
                params![
                    id,
                    t,
                    json!({"role": role, "parentID": parent, "time": {"created": t}}).to_string()
                ],
            )
            .unwrap();
        }
        let parts = [
            ("p1", "m1", json!({"type": "text", "text": "고쳐줘"})),
            (
                "p2",
                "m2",
                json!({"type": "tool", "tool": "edit", "state": {"output": "TOOL_DIFF_SECRET_123"}}),
            ),
            ("p3", "m3", json!({"type": "text", "text": "고쳤습니다"})),
            ("p4", "m4", json!({"type": "text", "text": "고마워"})),
            ("p5", "m5", json!({"type": "text", "text": "천만에요"})),
        ];
        for (id, mid, data) in parts {
            conn.execute(
                "INSERT INTO part VALUES (?1, 'ses_t', ?2, 1, 1, ?3)",
                params![id, mid, data.to_string()],
            )
            .unwrap();
        }
    }
    let opts = ParseOptions::default();
    let cand = wombat::parsers::opencode::discover(dir.path(), &opts)
        .unwrap()
        .remove(0);
    let ParseOutcome::Session(s) = parse_candidate(&cand, &opts).unwrap() else {
        panic!("expected session");
    };
    // The mirror itself never carries the tool output.
    assert!(!render_mirror(&s).content.contains("TOOL_DIFF_SECRET_123"));
    assert_eq!(s.turns.len(), 4);

    // Tracing the first reply recovers the earlier tool message of the same turn...
    let first_reply = extract_raw_trace(
        AgentKind::OpenCode,
        &s.source_path,
        &s.turns[1].source_offset,
        0,
    )
    .unwrap();
    assert!(
        first_reply.contains("TOOL_DIFF_SECRET_123"),
        "{first_reply}"
    );
    assert!(first_reply.contains("고쳤습니다"));
    // ...but not the next turn's messages.
    assert!(!first_reply.contains("천만에요"));
}

#[test]
fn opencode_v1_v2_sqlite_and_legacy_json_parsers_work() {
    let dir = TempDir::new().unwrap();

    // 1. SQLite with both v2 (`session_v2` + `session_message`) and v1 (`session` + `message` + `part`)
    let db_dir = dir.path().join("oc-sqlite");
    std::fs::create_dir_all(&db_dir).unwrap();
    let db_path = db_dir.join("opencode.db");
    {
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "
            CREATE TABLE session (id TEXT PRIMARY KEY, parent_id TEXT, directory TEXT, title TEXT, time_created INTEGER, time_updated INTEGER);
            CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);
            CREATE TABLE part (id TEXT PRIMARY KEY, session_id TEXT, message_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);
            CREATE TABLE session_v2 (id TEXT PRIMARY KEY, parent_id TEXT, directory TEXT, title TEXT, time_created INTEGER, time_updated INTEGER);
            CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT, seq INTEGER, type TEXT, time_created INTEGER, data TEXT);
            ",
        )
        .unwrap();

        // v1 session
        conn.execute(
            "INSERT INTO session VALUES ('ses_v1', NULL, '/Volumes/SSD1/1/wombat', 'OpenCode V1 Title', 1760000000000, 1760000050000)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message VALUES ('msg_u1', 'ses_v1', 1760000001000, 1760000001000, ?1)",
            params![
                json!({"id": "msg_u1", "role": "user", "time": {"created": 1760000001000i64}})
                    .to_string()
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part VALUES ('prt_1', 'ses_v1', 'msg_u1', 1, 1, ?1)",
            params![json!({"type": "text", "text": "OpenCode v1 질문입니다\n<!-- OMO_INTERNAL_INITIATOR -->"}).to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part VALUES ('prt_synth', 'ses_v1', 'msg_u1', 1, 1, ?1)",
            params![
                json!({"type": "text", "synthetic": true, "text": "SYNTHETIC_FILE_DUMP_SECRET"})
                    .to_string()
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message VALUES ('msg_a1', 'ses_v1', 1760000002000, 1760000002000, ?1)",
            params![
                json!({"id": "msg_a1", "role": "assistant", "time": {"created": 1760000002000i64}})
                    .to_string()
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part VALUES ('prt_tool', 'ses_v1', 'msg_a1', 2, 2, ?1)",
            params![
                json!({"type": "tool", "tool": "bash", "state": {"output": "OC_TOOL_SECRET"}})
                    .to_string()
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part VALUES ('prt_2', 'ses_v1', 'msg_a1', 2, 2, ?1)",
            params![json!({"type": "text", "text": "OpenCode v1 답변입니다."}).to_string()],
        )
        .unwrap();

        // v2 session + subagent child (child should be skipped)
        conn.execute(
            "INSERT INTO session_v2 VALUES ('ses_v2', NULL, '/Volumes/SSD1/1/AutoRAG', 'OpenCode V2 Title', 1760000100000, 1760000150000)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_v2 VALUES ('ses_v2_sub', 'ses_v2', '/Volumes/SSD1/1/AutoRAG', 'Child', 1760000100000, 1760000150000)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_message VALUES ('m2_u', 'ses_v2', 1, 'user', 1760000101000, ?1)",
            params![json!({"text": "OpenCode v2 SQLite 스키마 검증"}).to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_message VALUES ('m2_a', 'ses_v2', 2, 'assistant', 1760000102000, ?1)",
            params![json!({
                "content": [
                    {"type": "reasoning", "text": "OC_V2_REASONING_SECRET"},
                    {"type": "tool", "name": "edit", "state": {"input": "OC_V2_EDIT_DIFF"}},
                    {"type": "text", "text": "session_v2 테이블에서 대화만 추출했습니다."}
                ]
            }).to_string()],
        )
        .unwrap();
    }

    let opts = ParseOptions::default();
    let cands = wombat::parsers::opencode::discover(&db_dir, &opts).unwrap();
    assert_eq!(cands.len(), 2);
    for c in &cands {
        let ParseOutcome::Session(s) = parse_candidate(c, &opts).unwrap() else {
            panic!("expected opencode session for {:?}", c.inner_id);
        };
        assert_eq!(s.turns.len(), 2);
        let mirror = render_mirror(&s);
        assert!(!mirror.content.contains("SYNTHETIC_FILE_DUMP_SECRET"));
        assert!(!mirror.content.contains("OC_TOOL_SECRET"));
        assert!(!mirror.content.contains("OC_V2_REASONING_SECRET"));
        assert!(!mirror.content.contains("OMO_INTERNAL_INITIATOR"));

        // Verify SQLite item trace lookup!
        let trace = extract_raw_trace(
            AgentKind::OpenCode,
            &s.source_path,
            &s.turns[1].source_offset,
            0,
        )
        .unwrap();
        assert!(!trace.is_empty());
    }

    // 2. Legacy JSON file storage
    let legacy_root = dir.path().join("oc-legacy").join("storage");
    std::fs::create_dir_all(legacy_root.join("session/proj1")).unwrap();
    std::fs::create_dir_all(legacy_root.join("message/ses_leg")).unwrap();
    std::fs::create_dir_all(legacy_root.join("part/msg_lu")).unwrap();
    std::fs::write(
        legacy_root.join("session/proj1/ses_leg.json"),
        json!({"id": "ses_leg", "directory": "/Volumes/SSD1/1/katok", "time": {"created": 1760000200000i64}}).to_string(),
    )
    .unwrap();
    std::fs::write(
        legacy_root.join("message/ses_leg/msg_lu.json"),
        json!({"id": "msg_lu", "role": "user", "time": {"created": 1760000201000i64}}).to_string(),
    )
    .unwrap();
    std::fs::write(
        legacy_root.join("part/msg_lu/001.json"),
        json!({"type": "text", "text": "레거시 JSON 스토리지도 지원하나요?"}).to_string(),
    )
    .unwrap();

    let leg_cands =
        wombat::parsers::opencode::discover(&dir.path().join("oc-legacy"), &opts).unwrap();
    assert_eq!(leg_cands.len(), 1);
    let ParseOutcome::Session(leg_s) = parse_candidate(&leg_cands[0], &opts).unwrap() else {
        panic!("expected legacy opencode session");
    };
    assert_eq!(leg_s.turns[0].text, "레거시 JSON 스토리지도 지원하나요?");
}

#[test]
fn hermes_db_and_json_sessions_are_parsed_without_harness_noise() {
    let dir = TempDir::new().unwrap();
    let home = dir.path();
    {
        let conn = Connection::open(home.join("state.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, source TEXT NOT NULL, parent_session_id TEXT, started_at REAL NOT NULL, ended_at REAL, title TEXT, cwd TEXT);
             CREATE TABLE messages (id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL, role TEXT NOT NULL, content TEXT, tool_calls TEXT, tool_name TEXT, tool_call_id TEXT, timestamp REAL NOT NULL, active INTEGER NOT NULL DEFAULT 1, display_kind TEXT);
             INSERT INTO sessions VALUES ('s_chat', 'discord', NULL, 1777690945.5, 1777690999.0, 'T', '/Users/a/proj');
             INSERT INTO sessions VALUES ('s_cron', 'cron', NULL, 1777690945.5, NULL, NULL, '/Users/a');
             INSERT INTO messages (session_id, role, content, timestamp, active, display_kind) VALUES
               ('s_chat', 'user', '디스크 이름 바뀜', 1777690946.0, 1, NULL),
               ('s_chat', 'assistant', '', 1777690947.0, 1, NULL),
               ('s_chat', 'tool', '{\"output\": \"x\"}', 1777690948.0, 1, NULL),
               ('s_chat', 'assistant', '확인했다', 1777690949.0, 1, NULL),
               ('s_chat', 'user', '[System note: Your previous turn was interrupted]', 1777690950.0, 1, NULL),
               ('s_chat', 'assistant', '[CONTEXT COMPACTION — REFERENCE ONLY] summary', 1777690951.0, 1, NULL),
               ('s_chat', 'assistant', 'Gateway shutting down', 1777690952.0, 1, 'failed_turn'),
               ('s_chat', 'assistant', 'rewound away', 1777690953.0, 0, NULL),
               ('s_cron', 'user', 'run job', 1777690946.0, 1, NULL);",
        )
        .unwrap();
    }
    write_json(
        &home
            .join("sessions")
            .join("session_20260502_210359_6013e7.json"),
        &json!({
            "session_id": "20260502_210359_6013e7",
            "platform": "discord",
            "session_start": "2026-05-02T21:03:59.057973",
            "messages": [
                {"role": "user", "content": "json only question"},
                {"role": "assistant", "content": "", "tool_calls": []},
                {"role": "tool", "content": "out"},
                {"role": "assistant", "content": "json only answer"}
            ]
        }),
    );
    // Same id as the DB session: must not be emitted twice.
    write_json(
        &home.join("sessions").join("session_s_chat.json"),
        &json!({"messages": [{"role": "user", "content": "dup"}]}),
    );
    write_json(
        &home
            .join("sessions")
            .join("session_cron_abc_20260101_000000.json"),
        &json!({"platform": "cron", "messages": [{"role": "user", "content": "cron"}]}),
    );

    let opts = ParseOptions::default();
    let roots = SourceRoots {
        hermes: vec![home.to_path_buf()],
        ..Default::default()
    };
    let cands = discover_all(&roots, None, &opts).unwrap();
    let mut ids: Vec<_> = cands.iter().map(|c| c.inner_id.clone().unwrap()).collect();
    ids.sort();
    assert_eq!(ids, ["20260502_210359_6013e7", "s_chat"]);

    let by_id = |id: &str| {
        cands
            .iter()
            .find(|c| c.inner_id.as_deref() == Some(id))
            .unwrap()
    };
    let ParseOutcome::Session(s) = parse_candidate(by_id("s_chat"), &opts).unwrap() else {
        panic!("expected db session");
    };
    assert_eq!(s.agent, AgentKind::Hermes);
    assert_eq!(s.cwd.as_deref(), Some("/Users/a/proj"));
    assert_eq!(s.title.as_deref(), Some("T"));
    let texts: Vec<_> = s.turns.iter().map(|t| (t.role, t.text.as_str())).collect();
    assert_eq!(
        texts,
        [
            (Role::User, "디스크 이름 바뀜"),
            (Role::Assistant, "확인했다")
        ]
    );
    assert_eq!(
        s.turns[1].source_offset,
        SourceOffset::Item {
            item_id: "4".into()
        }
    );
    let raw = extract_raw_trace(
        AgentKind::Hermes,
        &s.source_path,
        &s.turns[1].source_offset,
        0,
    )
    .unwrap();
    assert!(raw.contains("\"role\": \"tool\"") && raw.contains("확인했다"));
    assert!(!raw.contains("디스크 이름 바뀜"));

    let ParseOutcome::Session(j) = parse_candidate(by_id("20260502_210359_6013e7"), &opts).unwrap()
    else {
        panic!("expected json session");
    };
    assert_eq!(j.turns.len(), 2);
    assert_eq!(j.turns[1].text, "json only answer");
    let raw = extract_raw_trace(
        AgentKind::Hermes,
        &j.source_path,
        &j.turns[1].source_offset,
        0,
    )
    .unwrap();
    assert!(raw.contains("\"role\": \"tool\"") && raw.contains("json only answer"));

    let with_cron = ParseOptions {
        include_ephemeral: true,
        ..Default::default()
    };
    assert_eq!(discover_all(&roots, None, &with_cron).unwrap().len(), 4);
}

fn write_json(path: &Path, value: &serde_json::Value) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, value.to_string()).unwrap();
}

fn aside_fixture(root: &Path) {
    let acct = root.join("u").join("0");
    {
        std::fs::create_dir_all(&acct).unwrap();
        let conn = Connection::open(acct.join("state.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, parent_id TEXT, title TEXT, trigger TEXT, ephemeral INTEGER NOT NULL DEFAULT 0, created_at INTEGER, updated_at INTEGER, cwd TEXT);
             INSERT INTO sessions VALUES ('ses_chat', NULL, 'ProctorU 확인', '{\"type\":\"user\",\"source\":\"sidepanel\"}', 0, 1793536840, 1793536999, '/Users/a/.aside/u/0');
             INSERT INTO sessions VALUES ('ses_sub', 'ses_chat', 'sub', '{\"type\":\"subagent\"}', 0, 1793536841, 1793536842, '/x');
             INSERT INTO sessions VALUES ('ses_routine', NULL, 'routine', '{\"type\":\"routine\",\"title\":\"r\"}', 0, 1793536843, 1793536844, '/x');
             INSERT INTO sessions VALUES ('ses_ephem', NULL, 'tmp', '{\"type\":\"user\"}', 1, 1793536845, 1793536846, '/x');",
        )
        .unwrap();
    }
    let user = |text: &str, ts: i64| json!({"role": "user", "content": text, "timestamp": ts});
    let asst = |blocks: serde_json::Value, ts: i64| json!({"role": "assistant", "content": blocks, "timestamp": ts, "stopReason": "stop"});
    write_jsonl_lines(
        &acct.join("sessions/2026-11-01_ses_chat/messages.jsonl"),
        &[
            json!({"role": "turn-lifecycle", "event": "started", "turnId": "t1", "timestamp": 1793536840000i64}),
            json!({"role": "user-message-metadata", "attachments": [], "timestamp": 1793536840100i64}),
            user("ProctorU 시험 예약 zeta_probe 찾아봐", 1793536840200),
            json!({"role": "system-message", "kind": "memory_recall", "content": "<recalled_memories>MEMORY_NOISE</recalled_memories>", "metadata": {}, "timestamp": 1793536840300i64}),
            asst(
                json!([{"type": "thinking", "thinking": "THINKING_NOISE"}, {"type": "toolCall", "id": "c1", "name": "repl", "arguments": {"code": "TOOLCALL_NOISE"}}]),
                1793536840400,
            ),
            json!({"role": "toolResult", "toolCallId": "c1", "toolName": "repl", "content": [{"type": "text", "text": "TOOLRESULT_NOISE"}], "timestamp": 1793536840500i64}),
            asst(
                json!([{"type": "thinking", "thinking": "THINKING_NOISE2"}, {"type": "text", "text": "예약 가능한 시험은 alpha_exam 입니다."}]),
                1793536840600,
            ),
            json!({"role": "steering", "id": "s1", "content": "추가 지시 beta_steer", "attachments": [], "timestamp": 1793536840650i64}),
            json!({"role": "user", "content": "추가 지시 beta_steer", "metadata": {"steer": true, "steeringMessageId": "s1"}, "timestamp": 1793536840651i64}),
            json!({"role": "compaction", "summary": "COMPACTION_NOISE", "timestamp": 1793536840700i64}),
            asst(
                json!([{"type": "thinking", "thinking": "only thinking"}]),
                1793536840800,
            ),
            json!({"role": "system-message", "kind": "routine_trigger", "content": "This turn was triggered by a heartbeat routine.", "metadata": {}, "timestamp": 1793536840900i64}),
            asst(
                json!([{"type": "text", "text": "HEARTBEAT_NOISE"}]),
                1793536841000,
            ),
            user("다음 질문 gamma_next", 1793536841100),
            asst(
                json!([{"type": "text", "text": "다음 답 delta_reply"}]),
                1793536841200,
            ),
        ],
    );
    write_jsonl_lines(
        &acct.join("sessions/2026-11-01_ses_sub/messages.jsonl"),
        &[
            user("subagent work", 1793536841000),
            asst(json!([{"type": "text", "text": "sub done"}]), 1793536841100),
        ],
    );
    write_jsonl_lines(
        &acct.join("sessions/2026-11-01_ses_routine/messages.jsonl"),
        &[
            user("scheduled check", 1793536842000),
            asst(
                json!([{"type": "text", "text": "routine done"}]),
                1793536842100,
            ),
        ],
    );
    write_jsonl_lines(
        &acct.join("sessions/2026-11-01_ses_ephem/messages.jsonl"),
        &[
            user("ephemeral q", 1793536843000),
            asst(
                json!([{"type": "text", "text": "ephemeral a"}]),
                1793536843100,
            ),
        ],
    );
    // Not in state.db: heuristics apply (forked worker prompt => subagent).
    write_jsonl_lines(
        &acct.join("sessions/2026-11-02_ses_nodb/messages.jsonl"),
        &[
            user("db 없는 일반 세션", 1793536844000),
            asst(json!([{"type": "text", "text": "ok nodb"}]), 1793536844100),
        ],
    );
    write_jsonl_lines(
        &acct.join("sessions/2026-11-02_ses_fork/messages.jsonl"),
        &[
            user("parent question replayed into the fork", 1793536844900),
            asst(
                json!([{"type": "text", "text": "parent answer replayed"}]),
                1793536844950,
            ),
            user(
                "<system_message>\nSTOP. READ THIS FIRST.\nFrom now on, you are a forked worker process.\n</system_message>",
                1793536845000,
            ),
            asst(
                json!([{"type": "text", "text": "fork reply"}]),
                1793536845100,
            ),
        ],
    );
    // Empty dialogue (lifecycle only) must be skipped, not fail.
    write_jsonl_lines(
        &acct.join("sessions/2026-11-02_ses_empty/messages.jsonl"),
        &[
            json!({"role": "turn-lifecycle", "event": "started", "turnId": "t", "timestamp": 1793536846000i64}),
        ],
    );
}

fn aside_ids(roots: &SourceRoots, opts: &ParseOptions) -> Vec<String> {
    let mut ids: Vec<String> = discover_all(roots, None, opts)
        .unwrap()
        .iter()
        .filter_map(|c| match parse_candidate(c, opts).unwrap() {
            ParseOutcome::Session(s) => Some(s.session_id),
            ParseOutcome::Skipped { .. } => None,
        })
        .collect();
    ids.sort();
    ids
}

#[test]
fn aside_keeps_only_user_and_assistant_text() {
    let dir = TempDir::new().unwrap();
    aside_fixture(dir.path());
    let roots = SourceRoots {
        aside: vec![dir.path().join("u")],
        ..Default::default()
    };
    let opts = ParseOptions::default();
    let cands = discover_all(&roots, None, &opts).unwrap();
    let chat = cands
        .iter()
        .find(|c| c.inner_id.as_deref() == Some("ses_chat") || c.source_path.contains("ses_chat"))
        .expect("chat session discovered");
    let ParseOutcome::Session(s) = parse_candidate(chat, &opts).unwrap() else {
        panic!("expected aside session");
    };
    assert_eq!(s.agent, AgentKind::Aside);
    assert_eq!(s.session_id, "ses_chat");
    assert_eq!(s.title.as_deref(), Some("ProctorU 확인"));
    assert_eq!(s.parent_session_id, None);
    assert!(!s.is_subagent);
    assert_eq!(s.started_at.as_deref(), Some("2026-11-01T12:40:40.000Z"));
    let got: Vec<_> = s.turns.iter().map(|t| (t.role, t.text.as_str())).collect();
    assert_eq!(
        got,
        [
            (Role::User, "ProctorU 시험 예약 zeta_probe 찾아봐"),
            (Role::Assistant, "예약 가능한 시험은 alpha_exam 입니다."),
            (Role::User, "추가 지시 beta_steer"),
            (Role::User, "다음 질문 gamma_next"),
            (Role::Assistant, "다음 답 delta_reply"),
        ]
    );
    assert_eq!(
        s.turns[1].source_offset,
        SourceOffset::Lines {
            start_line: 7,
            end_line: 7
        }
    );
    let mirror = render_mirror(&s).content;
    for noise in [
        "THINKING_NOISE",
        "TOOLCALL_NOISE",
        "TOOLRESULT_NOISE",
        "MEMORY_NOISE",
        "COMPACTION_NOISE",
        "HEARTBEAT_NOISE",
    ] {
        assert!(!mirror.contains(noise), "{noise} leaked into mirror");
    }
    // Raw trace of an assistant turn still exposes the tool calls for debugging.
    let raw = extract_raw_trace(
        AgentKind::Aside,
        &s.source_path,
        &s.turns[1].source_offset,
        3,
    )
    .unwrap();
    assert!(raw.contains("TOOLCALL_NOISE") && raw.contains("alpha_exam"));
}

#[test]
fn aside_classifies_subagent_routine_and_ephemeral_sessions() {
    let dir = TempDir::new().unwrap();
    aside_fixture(dir.path());
    let roots = SourceRoots {
        aside: vec![dir.path().join("u")],
        ..Default::default()
    };
    assert_eq!(
        aside_ids(&roots, &ParseOptions::default()),
        ["ses_chat", "ses_nodb"]
    );
    let subs = ParseOptions {
        include_subagents: true,
        ..Default::default()
    };
    assert_eq!(
        aside_ids(&roots, &subs),
        ["ses_chat", "ses_fork", "ses_nodb", "ses_sub"]
    );
    let eph = ParseOptions {
        include_ephemeral: true,
        ..Default::default()
    };
    assert_eq!(
        aside_ids(&roots, &eph),
        ["ses_chat", "ses_ephem", "ses_nodb", "ses_routine"]
    );
}

#[test]
fn aside_roots_resolve_from_home_account_or_sessions_dir() {
    let dir = TempDir::new().unwrap();
    aside_fixture(dir.path());
    let opts = ParseOptions::default();
    for root in [
        dir.path().to_path_buf(),
        dir.path().join("u"),
        dir.path().join("u/0"),
        dir.path().join("u/0/sessions"),
    ] {
        let roots = SourceRoots {
            aside: vec![root.clone()],
            ..Default::default()
        };
        assert_eq!(
            aside_ids(&roots, &opts),
            ["ses_chat", "ses_nodb"],
            "root {}",
            root.display()
        );
    }
}

fn gemini_msg_user(id: &str, text: &str, ts: &str) -> serde_json::Value {
    json!({"id": id, "timestamp": ts, "type": "user", "content": [{"text": text}]})
}

fn gemini_msg_assistant(id: &str, text: &str, ts: &str) -> serde_json::Value {
    json!({
        "id": id, "timestamp": ts, "type": "gemini", "content": text, "model": "gemini-3.5-flash",
        "thoughts": [{"subject": "GEMINI_THOUGHT_NOISE", "description": "GEMINI_THOUGHT_NOISE", "timestamp": ts}],
        "toolCalls": [{"id": "t1", "name": "run_shell_command", "args": {"command": "GEMINI_TOOLCALL_NOISE"}, "result": [{"functionResponse": {"response": {"output": "GEMINI_TOOLRESULT_NOISE"}}}], "status": "success"}]
    })
}

fn gemini_fixture(root: &Path) -> std::path::PathBuf {
    let chats = root.join("tmp").join("proj").join("chats");
    std::fs::create_dir_all(&chats).unwrap();
    let path = chats.join("session-2026-11-05T01-02-aaaa1111.jsonl");
    write_jsonl_lines(
        &path,
        &[
            json!({"sessionId": "aaaa1111-0000-0000-0000-000000000000", "projectHash": "h", "startTime": "2026-11-05T01:02:03.000Z", "lastUpdated": "2026-11-05T01:02:03.000Z", "kind": "main", "directories": ["/Users/a/proj"]}),
            gemini_msg_user(
                "u0",
                "<session_context>\nThis is the Gemini CLI. GEMINI_CONTEXT_NOISE\n</session_context>",
                "2026-11-05T01:02:03.100Z",
            ),
            gemini_msg_user("u1", "alpha_gemini 가 뭐야?", "2026-11-05T01:02:04.000Z"),
            gemini_msg_assistant(
                "g1",
                "alpha_gemini는 테스트 용어입니다.",
                "2026-11-05T01:02:05.000Z",
            ),
            json!({"id": "i1", "timestamp": "2026-11-05T01:02:05.500Z", "type": "info", "content": "GEMINI_INFO_NOISE"}),
            gemini_msg_user("u2", "/help", "2026-11-05T01:02:06.000Z"),
            gemini_msg_user(
                "u3",
                "rewound_question beta_gemini",
                "2026-11-05T01:02:07.000Z",
            ),
            gemini_msg_assistant("g2", "rewound_answer", "2026-11-05T01:02:08.000Z"),
            json!({"$rewindTo": "u3"}),
            gemini_msg_user("u3b", "gamma_gemini 다시 질문", "2026-11-05T01:02:09.000Z"),
            // tool-only turn: no visible text
            json!({"id": "g3", "timestamp": "2026-11-05T01:02:10.000Z", "type": "gemini", "content": "", "toolCalls": [{"id": "t2", "name": "read_file", "args": {}}]}),
            // later message arrives as a patch that fills in the final text
            json!({"id": "g4", "timestamp": "2026-11-05T01:02:11.000Z", "type": "gemini", "content": "초안", "toolCalls": []}),
            json!({"$patch": {"id": "g4", "content": "delta_gemini 최종 답변"}}),
            json!({"$set": {"lastUpdated": "2026-11-05T01:02:11.000Z", "summary": "ignored summary"}}),
        ],
    );
    path
}

#[test]
fn gemini_keeps_only_user_and_assistant_text_and_honors_rewind_patch() {
    let dir = TempDir::new().unwrap();
    let path = gemini_fixture(dir.path());
    let roots = SourceRoots {
        gemini: vec![dir.path().to_path_buf()],
        ..Default::default()
    };
    let opts = ParseOptions::default();
    let cands = discover_all(&roots, None, &opts).unwrap();
    assert_eq!(cands.len(), 1);
    let ParseOutcome::Session(s) = parse_candidate(&cands[0], &opts).unwrap() else {
        panic!("expected gemini session");
    };
    assert_eq!(s.agent, AgentKind::Gemini);
    assert_eq!(s.session_id, "aaaa1111-0000-0000-0000-000000000000");
    assert_eq!(s.cwd.as_deref(), Some("/Users/a/proj"));
    assert_eq!(s.started_at.as_deref(), Some("2026-11-05T01:02:03.000Z"));
    assert_eq!(s.source_path, path.to_string_lossy());
    let got: Vec<_> = s.turns.iter().map(|t| (t.role, t.text.as_str())).collect();
    assert_eq!(
        got,
        [
            (Role::User, "alpha_gemini 가 뭐야?"),
            (Role::Assistant, "alpha_gemini는 테스트 용어입니다."),
            (Role::User, "gamma_gemini 다시 질문"),
            (Role::Assistant, "delta_gemini 최종 답변"),
        ]
    );
    // Offsets point at the raw line holding the (final) message record.
    assert_eq!(
        s.turns[0].source_offset,
        SourceOffset::Lines {
            start_line: 3,
            end_line: 3
        }
    );
    let mirror = render_mirror(&s).content;
    for noise in [
        "GEMINI_CONTEXT_NOISE",
        "GEMINI_THOUGHT_NOISE",
        "GEMINI_TOOLCALL_NOISE",
        "GEMINI_TOOLRESULT_NOISE",
        "GEMINI_INFO_NOISE",
        "rewound_question",
        "rewound_answer",
        "ignored summary",
    ] {
        assert!(!mirror.contains(noise), "{noise} leaked into mirror");
    }
    let raw = extract_raw_trace(
        AgentKind::Gemini,
        &s.source_path,
        &s.turns[1].source_offset,
        0,
    )
    .unwrap();
    assert!(raw.contains("GEMINI_TOOLCALL_NOISE"));
}

#[test]
fn gemini_reads_set_messages_snapshots_and_skips_subagent_kind() {
    let dir = TempDir::new().unwrap();
    let chats = dir.path().join("tmp/proj/chats");
    write_jsonl_lines(
        &chats.join("session-2026-11-06T00-00-bbbb2222.jsonl"),
        &[
            json!({"sessionId": "bbbb2222", "projectHash": "h", "startTime": "2026-11-06T00:00:00.000Z", "lastUpdated": "2026-11-06T00:00:00.000Z", "kind": "main"}),
            json!({"$set": {"messages": [
                gemini_msg_user("u1", "snapshot_question", "2026-11-06T00:00:01.000Z"),
                gemini_msg_assistant("g1", "snapshot_answer", "2026-11-06T00:00:02.000Z"),
            ], "lastUpdated": "2026-11-06T00:00:02.000Z"}}),
        ],
    );
    write_jsonl_lines(
        &chats.join("session-2026-11-06T00-01-cccc3333.jsonl"),
        &[
            json!({"sessionId": "cccc3333", "projectHash": "h", "startTime": "2026-11-06T00:01:00.000Z", "lastUpdated": "2026-11-06T00:01:00.000Z", "kind": "subagent"}),
            gemini_msg_user("u1", "worker task", "2026-11-06T00:01:01.000Z"),
            gemini_msg_assistant("g1", "worker done", "2026-11-06T00:01:02.000Z"),
        ],
    );
    let roots = SourceRoots {
        gemini: vec![dir.path().to_path_buf()],
        ..Default::default()
    };
    let ids = |opts: &ParseOptions| {
        let mut v: Vec<String> = discover_all(&roots, None, opts)
            .unwrap()
            .iter()
            .filter_map(|c| match parse_candidate(c, opts).unwrap() {
                ParseOutcome::Session(s) => {
                    let first = s.turns[0].text.clone();
                    Some(format!("{}:{}", s.session_id, first))
                }
                ParseOutcome::Skipped { .. } => None,
            })
            .collect();
        v.sort();
        v
    };
    assert_eq!(
        ids(&ParseOptions::default()),
        ["bbbb2222:snapshot_question"]
    );
    let subs = ParseOptions {
        include_subagents: true,
        ..Default::default()
    };
    assert_eq!(
        ids(&subs),
        ["bbbb2222:snapshot_question", "cccc3333:worker task"]
    );
}

#[test]
fn gemini_roots_resolve_from_home_tmp_or_chats_dir() {
    let dir = TempDir::new().unwrap();
    gemini_fixture(dir.path());
    for root in [
        dir.path().to_path_buf(),
        dir.path().join("tmp"),
        dir.path().join("tmp/proj"),
        dir.path().join("tmp/proj/chats"),
    ] {
        let roots = SourceRoots {
            gemini: vec![root.clone()],
            ..Default::default()
        };
        assert_eq!(
            discover_all(&roots, None, &ParseOptions::default())
                .unwrap()
                .len(),
            1,
            "root {}",
            root.display()
        );
    }
}

/// Kilo Code 7.8 stores sessions in `<data>/kilo/kilo.db` using the OpenCode v1 tables
/// (`session`/`message`/`part`); columns and JSON shapes below match a live TUI run.
fn kilo_fixture(dir: &Path) -> std::path::PathBuf {
    let db_path = dir.join("kilo.db");
    let conn = Connection::open(&db_path).unwrap();
    conn.execute_batch(
        "CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT NOT NULL, parent_id TEXT, slug TEXT NOT NULL, directory TEXT NOT NULL, title TEXT NOT NULL, version TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, time_archived INTEGER);
         CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);
         CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL, session_id TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);",
    )
    .unwrap();
    conn.execute("INSERT INTO session VALUES ('ses_k1', 'p', NULL, 's', '/Users/a/proj', 'kilo chat', '7.8.27', 1794577663297, 1794577679646, NULL)", []).unwrap();
    conn.execute("INSERT INTO session VALUES ('ses_k2', 'p', 'ses_k1', 's2', '/Users/a/proj', 'child', '7.8.27', 1794577663300, 1794577679600, NULL)", []).unwrap();
    let msg = |id: &str, sid: &str, ts: i64, data: serde_json::Value| {
        conn.execute(
            "INSERT INTO message VALUES (?1, ?2, ?3, ?3, ?4)",
            params![id, sid, ts, data.to_string()],
        )
        .unwrap();
    };
    let part = |id: &str, mid: &str, sid: &str, ts: i64, data: serde_json::Value| {
        conn.execute(
            "INSERT INTO part VALUES (?1, ?2, ?3, ?4, ?4, ?5)",
            params![id, mid, sid, ts, data.to_string()],
        )
        .unwrap();
    };
    msg(
        "m1",
        "ses_k1",
        1794577663321,
        json!({"role": "user", "time": {"created": 1794577663321i64}, "agent": "code"}),
    );
    part(
        "p1",
        "m1",
        "ses_k1",
        1794577663322,
        json!({"type": "text", "text": "kilo_alpha 가 뭐야?"}),
    );
    msg(
        "m2",
        "ses_k1",
        1794577663400,
        json!({"role": "assistant", "parentID": "m1", "time": {"created": 1794577663400i64}}),
    );
    part(
        "p2",
        "m2",
        "ses_k1",
        1794577663401,
        json!({"type": "step-start"}),
    );
    part(
        "p3",
        "m2",
        "ses_k1",
        1794577663402,
        json!({"type": "reasoning", "text": "KILO_REASONING_NOISE"}),
    );
    part(
        "p4",
        "m2",
        "ses_k1",
        1794577663403,
        json!({"type": "tool", "tool": "bash", "callID": "c1", "state": {"status": "completed", "input": {"command": "KILO_TOOLCALL_NOISE"}, "output": "KILO_TOOLRESULT_NOISE"}}),
    );
    part(
        "p5",
        "m2",
        "ses_k1",
        1794577663404,
        json!({"type": "text", "text": "kilo_alpha는 테스트 용어입니다."}),
    );
    part(
        "p6",
        "m2",
        "ses_k1",
        1794577663405,
        json!({"type": "step-finish", "reason": "stop"}),
    );
    msg(
        "m3",
        "ses_k2",
        1794577663500,
        json!({"role": "user", "time": {"created": 1794577663500i64}}),
    );
    part(
        "p7",
        "m3",
        "ses_k2",
        1794577663501,
        json!({"type": "text", "text": "child task"}),
    );
    db_path
}

#[test]
fn kilo_reads_kilo_db_as_its_own_agent_with_opencode_semantics() {
    let dir = TempDir::new().unwrap();
    kilo_fixture(dir.path());
    let roots = SourceRoots {
        kilo: vec![dir.path().to_path_buf()],
        ..Default::default()
    };
    let opts = ParseOptions {
        include_ephemeral: true,
        ..Default::default()
    };
    let cands = discover_all(&roots, None, &opts).unwrap();
    assert!(cands.iter().all(|c| c.agent == AgentKind::Kilo));
    assert_eq!(cands.len(), 1, "child session skipped by default");
    assert!(cands[0].item_key.starts_with("kilo/"));
    let ParseOutcome::Session(s) = parse_candidate(&cands[0], &opts).unwrap() else {
        panic!("expected kilo session");
    };
    assert_eq!(s.agent, AgentKind::Kilo);
    assert_eq!(s.session_id, "ses_k1");
    assert_eq!(s.catalog_key(), "kilo:ses_k1");
    let got: Vec<_> = s.turns.iter().map(|t| (t.role, t.text.as_str())).collect();
    assert_eq!(
        got,
        [
            (Role::User, "kilo_alpha 가 뭐야?"),
            (Role::Assistant, "kilo_alpha는 테스트 용어입니다."),
        ]
    );
    let mirror = render_mirror(&s).content;
    for n in [
        "KILO_REASONING_NOISE",
        "KILO_TOOLCALL_NOISE",
        "KILO_TOOLRESULT_NOISE",
    ] {
        assert!(!mirror.contains(n), "{n} leaked");
    }
    let raw = extract_raw_trace(
        AgentKind::Kilo,
        &s.source_path,
        &s.turns[1].source_offset,
        0,
    )
    .unwrap();
    assert!(raw.contains("KILO_TOOLCALL_NOISE"), "{raw}");
}

#[test]
fn kilo_roots_accept_data_dir_or_db_file_and_do_not_capture_opencode_dbs() {
    let dir = TempDir::new().unwrap();
    kilo_fixture(dir.path());
    // An OpenCode DB living in the same folder must stay an OpenCode source.
    std::fs::copy(dir.path().join("kilo.db"), dir.path().join("opencode.db")).unwrap();
    let opts = ParseOptions {
        include_ephemeral: true,
        ..Default::default()
    };
    for root in [dir.path().to_path_buf(), dir.path().join("kilo.db")] {
        let roots = SourceRoots {
            kilo: vec![root.clone()],
            ..Default::default()
        };
        let cands = discover_all(&roots, None, &opts).unwrap();
        assert_eq!(cands.len(), 1, "root {}", root.display());
        assert_eq!(cands[0].agent, AgentKind::Kilo);
    }
    let oc = SourceRoots {
        opencode: vec![dir.path().to_path_buf()],
        ..Default::default()
    };
    let cands = discover_all(&oc, None, &opts).unwrap();
    assert_eq!(cands.len(), 1);
    assert_eq!(cands[0].agent, AgentKind::OpenCode);
    assert!(cands[0].source_path.contains("opencode.db"));
}

fn qwen_home(dir: &Path) -> std::path::PathBuf {
    let chats = dir.join("projects/-private-tmp-qa-proj/chats");
    std::fs::create_dir_all(&chats).unwrap();
    chats
}

#[test]
fn qwen_real_recorded_session_keeps_only_real_user_and_assistant_text() {
    // Records copied verbatim from a session the real Qwen Code 0.27.3 TUI wrote. The
    // `memory_catalog_snapshot` record has message.role == "user" but type == "system".
    let dir = TempDir::new().unwrap();
    let chats = qwen_home(dir.path());
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/qwen_real_session.jsonl"),
        chats.join("dc7dbb31-24ca-4ab6-83eb-6b371570a240.jsonl"),
    )
    .unwrap();
    let roots = SourceRoots {
        qwen: vec![dir.path().to_path_buf()],
        ..Default::default()
    };
    let opts = ParseOptions {
        include_ephemeral: true, // fixture cwd is under /private/tmp
        ..Default::default()
    };
    let cands = discover_all(&roots, None, &opts).unwrap();
    assert_eq!(cands.len(), 1);
    let ParseOutcome::Session(s) = parse_candidate(&cands[0], &opts).unwrap() else {
        panic!("expected qwen session");
    };
    assert_eq!(s.agent, AgentKind::Qwen);
    assert_eq!(s.session_id, "dc7dbb31-24ca-4ab6-83eb-6b371570a240");
    assert_eq!(s.cwd.as_deref(), Some("/private/tmp/qa-home-qwen/proj"));
    let got: Vec<_> = s.turns.iter().map(|t| (t.role, t.text.as_str())).collect();
    assert_eq!(got.len(), 2, "{got:?}");
    assert_eq!(got[0], (Role::User, "qa_qwen_alpha 라는 단어를 설명해줘"));
    assert_eq!(got[1].0, Role::Assistant);
    assert!(got[1].1.starts_with("MOCKREPLY 응답입니다:"));
    let mirror = render_mirror(&s).content;
    assert!(!mirror.contains("memory catalog"), "system snapshot leaked");
}

#[test]
fn qwen_skips_thought_tool_and_system_records_and_ephemeral_cwd() {
    let dir = TempDir::new().unwrap();
    let chats = qwen_home(dir.path());
    let rec = |uuid: &str,
               typ: &str,
               role: &str,
               parts: serde_json::Value,
               extra: serde_json::Value| {
        let mut v = json!({"uuid": uuid, "sessionId": "qwen-s1", "timestamp": "2026-11-13T00:00:00.000Z",
            "type": typ, "cwd": "/Users/a/proj", "version": "0.27.3",
            "message": {"role": role, "parts": parts}});
        if let (Some(o), Some(e)) = (v.as_object_mut(), extra.as_object()) {
            o.extend(e.clone());
        }
        v
    };
    write_jsonl_lines(
        &chats.join("qwen-s1.jsonl"),
        &[
            rec(
                "1",
                "user",
                "user",
                json!([{"text": "첫 질문 qwen_q1"}]),
                json!({"provenance": "real_user"}),
            ),
            rec(
                "2",
                "assistant",
                "model",
                json!([{"text": "QWEN_THOUGHT_NOISE", "thought": true}, {"text": "답변 qwen_a1"}]),
                json!({}),
            ),
            rec(
                "3",
                "assistant",
                "model",
                json!([{"functionCall": {"name": "run_shell_command", "args": {"command": "QWEN_TOOLCALL_NOISE"}}}]),
                json!({}),
            ),
            rec(
                "4",
                "tool_result",
                "user",
                json!([{"functionResponse": {"name": "run_shell_command", "response": {"output": "QWEN_TOOLRESULT_NOISE"}}}]),
                json!({}),
            ),
            rec(
                "5",
                "system",
                "user",
                json!([{"text": "QWEN_SYSTEM_SNAPSHOT_NOISE"}]),
                json!({"subtype": "memory_catalog_snapshot"}),
            ),
            rec(
                "6",
                "user",
                "user",
                json!([{"text": "/help"}]),
                json!({"provenance": "real_user"}),
            ),
            rec(
                "7",
                "user",
                "user",
                json!([{"text": "두번째 질문 qwen_q2"}]),
                json!({"provenance": "real_user"}),
            ),
        ],
    );
    write_jsonl_lines(
        &chats.join("qwen-tmp.jsonl"),
        &[
            json!({"uuid": "1", "sessionId": "qwen-tmp", "timestamp": "2026-11-13T00:00:00.000Z", "type": "user", "cwd": "/tmp/scratch", "version": "0.27.3", "message": {"role": "user", "parts": [{"text": "tmp question"}]}, "provenance": "real_user"}),
        ],
    );
    let roots = SourceRoots {
        qwen: vec![dir.path().to_path_buf()],
        ..Default::default()
    };
    let opts = ParseOptions::default();
    let sessions: Vec<_> = discover_all(&roots, None, &opts)
        .unwrap()
        .iter()
        .filter_map(|c| match parse_candidate(c, &opts).unwrap() {
            ParseOutcome::Session(s) => Some(s),
            ParseOutcome::Skipped { .. } => None,
        })
        .collect();
    assert_eq!(sessions.len(), 1, "tmp-cwd session must be skipped");
    let s = &sessions[0];
    let got: Vec<_> = s.turns.iter().map(|t| (t.role, t.text.as_str())).collect();
    assert_eq!(
        got,
        [
            (Role::User, "첫 질문 qwen_q1"),
            (Role::Assistant, "답변 qwen_a1"),
            (Role::User, "두번째 질문 qwen_q2"),
        ]
    );
    let mirror = render_mirror(s).content;
    for noise in [
        "QWEN_THOUGHT_NOISE",
        "QWEN_TOOLCALL_NOISE",
        "QWEN_TOOLRESULT_NOISE",
        "QWEN_SYSTEM_SNAPSHOT_NOISE",
    ] {
        assert!(!mirror.contains(noise), "{noise} leaked");
    }
    let raw = extract_raw_trace(
        AgentKind::Qwen,
        &s.source_path,
        &s.turns[1].source_offset,
        0,
    )
    .unwrap();
    assert!(raw.contains("QWEN_THOUGHT_NOISE"));
}

#[test]
fn qwen_roots_resolve_from_home_projects_or_chats_dir() {
    let dir = TempDir::new().unwrap();
    let chats = qwen_home(dir.path());
    write_jsonl_lines(
        &chats.join("s.jsonl"),
        &[
            json!({"uuid": "1", "sessionId": "s", "timestamp": "2026-11-13T00:00:00.000Z", "type": "user", "cwd": "/Users/a/p", "message": {"role": "user", "parts": [{"text": "hi"}]}, "provenance": "real_user"}),
        ],
    );
    for root in [
        dir.path().to_path_buf(),
        dir.path().join("projects"),
        dir.path().join("projects/-private-tmp-qa-proj"),
        chats.clone(),
    ] {
        let roots = SourceRoots {
            qwen: vec![root.clone()],
            ..Default::default()
        };
        assert_eq!(
            discover_all(&roots, None, &ParseOptions::default())
                .unwrap()
                .len(),
            1,
            "root {}",
            root.display()
        );
    }
}

/// Builds a Crush data dir like the real one: `<root>/projects.json` index and a per-project
/// `.crush/crush.db`. Schema and part JSON match what Crush 0.101 wrote in a live TUI run.
fn crush_fixture(root: &Path, project: &str) -> std::path::PathBuf {
    let data_dir = root.join(project).join(".crush");
    std::fs::create_dir_all(&data_dir).unwrap();
    let conn = Connection::open(data_dir.join("crush.db")).unwrap();
    conn.execute_batch(
        "CREATE TABLE sessions (id TEXT PRIMARY KEY, parent_session_id TEXT, title TEXT NOT NULL, message_count INTEGER NOT NULL DEFAULT 0, prompt_tokens INTEGER NOT NULL DEFAULT 0, completion_tokens INTEGER NOT NULL DEFAULT 0, cost REAL NOT NULL DEFAULT 0.0, updated_at INTEGER NOT NULL, created_at INTEGER NOT NULL, summary_message_id TEXT, todos TEXT, channel TEXT);
         CREATE TABLE messages (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, role TEXT NOT NULL, parts TEXT NOT NULL default '[]', model TEXT, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, finished_at INTEGER, provider TEXT, is_summary_message INTEGER DEFAULT 0 NOT NULL);",
    )
    .unwrap();
    conn.execute("INSERT INTO sessions (id, parent_session_id, title, updated_at, created_at) VALUES ('cs1', NULL, 'Crush chat', 1794576811, 1794576798)", []).unwrap();
    conn.execute("INSERT INTO sessions (id, parent_session_id, title, updated_at, created_at) VALUES ('cs-agent', 'cs1', 'Sub task', 1794576812, 1794576800)", []).unwrap();
    let msg = |id: &str, sid: &str, role: &str, parts: serde_json::Value, ts: i64, summary: i64| {
        conn.execute(
            "INSERT INTO messages (id, session_id, role, parts, created_at, updated_at, is_summary_message) VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6)",
            params![id, sid, role, parts.to_string(), ts, summary],
        )
        .unwrap();
    };
    msg(
        "m1",
        "cs1",
        "user",
        json!([{"type": "text", "data": {"text": "crush_alpha 가 뭐야?"}}, {"type": "finish", "data": {"reason": "stop", "time": 0}}]),
        1794576798,
        0,
    );
    msg(
        "m2",
        "cs1",
        "assistant",
        json!([{"type": "reasoning", "data": {"thinking": "CRUSH_THINKING_NOISE"}}, {"type": "tool_call", "data": {"id": "t1", "name": "bash", "input": "CRUSH_TOOLCALL_NOISE", "finished": true}}, {"type": "finish", "data": {"reason": "tool_use", "time": 1794576799}}]),
        1794576799,
        0,
    );
    msg(
        "m3",
        "cs1",
        "tool",
        json!([{"type": "tool_result", "data": {"tool_call_id": "t1", "name": "bash", "content": "CRUSH_TOOLRESULT_NOISE"}}]),
        1794576800,
        0,
    );
    msg(
        "m4",
        "cs1",
        "assistant",
        json!([{"type": "text", "data": {"text": "crush_alpha는 테스트 용어입니다."}}, {"type": "finish", "data": {"reason": "end_turn", "time": 1794576801}}]),
        1794576801,
        0,
    );
    msg(
        "m5",
        "cs1",
        "assistant",
        json!([{"type": "text", "data": {"text": "CRUSH_SUMMARY_NOISE"}}]),
        1794576802,
        1,
    );
    msg(
        "m6",
        "cs1",
        "user",
        json!([{"type": "text", "data": {"text": "crush_beta 두번째"}}]),
        1794576803,
        0,
    );
    msg(
        "m7",
        "cs-agent",
        "user",
        json!([{"type": "text", "data": {"text": "subtask prompt"}}]),
        1794576800,
        0,
    );
    msg(
        "m8",
        "cs-agent",
        "assistant",
        json!([{"type": "text", "data": {"text": "subtask answer"}}]),
        1794576801,
        0,
    );
    let share = root.join("share");
    std::fs::create_dir_all(&share).unwrap();
    std::fs::write(
        share.join("projects.json"),
        json!({"projects": [{"path": root.join(project), "data_dir": data_dir, "last_accessed": "2026-11-13T13:33:04Z"}]}).to_string(),
    )
    .unwrap();
    share
}

#[test]
fn crush_reads_text_parts_and_skips_reasoning_tools_summary_and_child_sessions() {
    let dir = TempDir::new().unwrap();
    let share = crush_fixture(dir.path(), "myproj");
    let roots = SourceRoots {
        crush: vec![share.clone()],
        ..Default::default()
    };
    // The fixture lives under the OS temp dir, which wombat treats as an ephemeral project.
    let default_ids = SourceRoots {
        crush: vec![share.clone()],
        ..Default::default()
    };
    assert!(
        discover_all(&default_ids, None, &ParseOptions::default())
            .unwrap()
            .iter()
            .all(|c| matches!(
                parse_candidate(c, &ParseOptions::default()).unwrap(),
                ParseOutcome::Skipped {
                    reason: "ephemeral_cwd"
                }
            )),
        "temp-dir project must be skipped by default"
    );
    let opts = ParseOptions {
        include_ephemeral: true,
        ..Default::default()
    };
    let cands = discover_all(&roots, None, &opts).unwrap();
    let sessions: Vec<_> = cands
        .iter()
        .filter_map(|c| match parse_candidate(c, &opts).unwrap() {
            ParseOutcome::Session(s) => Some(s),
            ParseOutcome::Skipped { .. } => None,
        })
        .collect();
    assert_eq!(sessions.len(), 1, "child (agent) session must be skipped");
    let s = &sessions[0];
    assert_eq!(s.agent, AgentKind::Crush);
    assert_eq!(s.session_id, "cs1");
    assert_eq!(s.title.as_deref(), Some("Crush chat"));
    assert!(s.cwd.as_deref().unwrap().ends_with("myproj"));
    let got: Vec<_> = s.turns.iter().map(|t| (t.role, t.text.as_str())).collect();
    assert_eq!(
        got,
        [
            (Role::User, "crush_alpha 가 뭐야?"),
            (Role::Assistant, "crush_alpha는 테스트 용어입니다."),
            (Role::User, "crush_beta 두번째"),
        ]
    );
    let mirror = render_mirror(s).content;
    for noise in [
        "CRUSH_THINKING_NOISE",
        "CRUSH_TOOLCALL_NOISE",
        "CRUSH_TOOLRESULT_NOISE",
        "CRUSH_SUMMARY_NOISE",
    ] {
        assert!(!mirror.contains(noise), "{noise} leaked");
    }
    // Trace of an assistant turn reaches back over the tool messages of that turn.
    let raw = extract_raw_trace(
        AgentKind::Crush,
        &s.source_path,
        &s.turns[1].source_offset,
        0,
    )
    .unwrap();
    assert!(raw.contains("CRUSH_TOOLCALL_NOISE") && raw.contains("crush_alpha는"));
}

#[test]
fn crush_include_subagents_returns_child_sessions_and_roots_accept_project_dirs() {
    let dir = TempDir::new().unwrap();
    let share = crush_fixture(dir.path(), "myproj");
    let subs = ParseOptions {
        include_subagents: true,
        include_ephemeral: true,
    };
    let roots = SourceRoots {
        crush: vec![share.clone()],
        ..Default::default()
    };
    let ids = |opts: &ParseOptions, roots: &SourceRoots| {
        let mut v: Vec<String> = discover_all(roots, None, opts)
            .unwrap()
            .iter()
            .filter_map(|c| match parse_candidate(c, opts).unwrap() {
                ParseOutcome::Session(s) => Some(s.session_id),
                ParseOutcome::Skipped { .. } => None,
            })
            .collect();
        v.sort();
        v
    };
    assert_eq!(ids(&subs, &roots), ["cs-agent", "cs1"]);
    // A project dir, its .crush dir, or the db file itself also work as a root.
    for root in [
        dir.path().join("myproj"),
        dir.path().join("myproj/.crush"),
        dir.path().join("myproj/.crush/crush.db"),
    ] {
        let r = SourceRoots {
            crush: vec![root.clone()],
            ..Default::default()
        };
        assert_eq!(
            ids(
                &ParseOptions {
                    include_ephemeral: true,
                    ..Default::default()
                },
                &r
            ),
            ["cs1"],
            "root {}",
            root.display()
        );
    }
}

#[test]
fn crush_trace_uses_row_order_when_turns_share_a_second() {
    // Crush stores second-resolution `created_at`; a prompt and its reply routinely share one.
    let dir = TempDir::new().unwrap();
    let data_dir = dir.path().join("p/.crush");
    std::fs::create_dir_all(&data_dir).unwrap();
    {
        let conn = Connection::open(data_dir.join("crush.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, parent_session_id TEXT, title TEXT NOT NULL, message_count INTEGER NOT NULL DEFAULT 0, updated_at INTEGER NOT NULL, created_at INTEGER NOT NULL);
             CREATE TABLE messages (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, role TEXT NOT NULL, parts TEXT NOT NULL DEFAULT '[]', model TEXT, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, is_summary_message INTEGER DEFAULT 0 NOT NULL);
             INSERT INTO sessions VALUES ('s', NULL, 't', 0, 100, 100);",
        )
        .unwrap();
        for (id, role, parts) in [
            (
                "a1",
                "user",
                json!([{"type": "text", "data": {"text": "first question"}}]),
            ),
            (
                "a2",
                "assistant",
                json!([{"type": "text", "data": {"text": "first answer"}}]),
            ),
            (
                "a3",
                "user",
                json!([{"type": "text", "data": {"text": "SECOND_Q"}}]),
            ),
            (
                "a4",
                "assistant",
                json!([{"type": "tool_call", "data": {"name": "SECOND_TOOLCALL"}}]),
            ),
            (
                "a5",
                "assistant",
                json!([{"type": "text", "data": {"text": "second answer"}}]),
            ),
        ] {
            conn.execute(
                "INSERT INTO messages (id, session_id, role, parts, created_at, updated_at) VALUES (?1, 's', ?2, ?3, 100, 100)",
                params![id, role, parts.to_string()],
            )
            .unwrap();
        }
    }
    let src = format!("{}#s", data_dir.join("crush.db").display());
    let raw = extract_raw_trace(
        AgentKind::Crush,
        &src,
        &SourceOffset::Item {
            item_id: "a5".into(),
        },
        0,
    )
    .unwrap();
    assert!(
        raw.contains("SECOND_TOOLCALL") && raw.contains("second answer"),
        "{raw}"
    );
    assert!(
        !raw.contains("first answer") && !raw.contains("first question"),
        "{raw}"
    );
    assert!(!raw.contains("SECOND_Q") || raw.contains("second answer"));
}

fn copilot_session_dir(home: &Path, id: &str) -> std::path::PathBuf {
    let d = home.join("session-state").join(id);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn copilot_real_recorded_session_keeps_user_and_assistant_text_only() {
    // Records copied verbatim from a session the real Copilot CLI 1.0.110 wrote (BYOK, offline).
    let dir = TempDir::new().unwrap();
    let id = "e9fd5251-fe21-4bd6-8647-9df3f3110136";
    let sd = copilot_session_dir(dir.path(), id);
    let fx = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    std::fs::copy(
        fx.join("copilot_real_events.jsonl"),
        sd.join("events.jsonl"),
    )
    .unwrap();
    std::fs::copy(
        fx.join("copilot_real_workspace.yaml"),
        sd.join("workspace.yaml"),
    )
    .unwrap();
    let roots = SourceRoots {
        copilot: vec![dir.path().to_path_buf()],
        ..Default::default()
    };
    let opts = ParseOptions {
        include_ephemeral: true, // fixture cwd is under /private/tmp
        ..Default::default()
    };
    let cands = discover_all(&roots, None, &opts).unwrap();
    assert_eq!(cands.len(), 1);
    let ParseOutcome::Session(s) = parse_candidate(&cands[0], &opts).unwrap() else {
        panic!("expected copilot session");
    };
    assert_eq!(s.agent, AgentKind::Copilot);
    assert_eq!(s.session_id, id);
    assert_eq!(s.cwd.as_deref(), Some("/private/tmp/qa-home-copilot/proj"));
    assert_eq!(
        s.title.as_deref(),
        Some("qa_copilot_alpha 라는 단어를 설명해줘")
    );
    let got: Vec<_> = s.turns.iter().map(|t| (t.role, t.text.as_str())).collect();
    // user, assistant(text), user, [assistant with only tool requests => dropped], assistant(text)
    assert_eq!(got.len(), 4, "{got:?}");
    assert_eq!(
        got[0],
        (Role::User, "qa_copilot_alpha 라는 단어를 설명해줘")
    );
    assert_eq!(
        got[2],
        (Role::User, "TOOLTEST qa_copilot_delta 셸로 echo 실행해줘")
    );
    assert!(got[1].1.starts_with("MOCKREPLY") && got[3].1.starts_with("MOCKREPLY"));
    // `content` is what the user typed; `transformedContent` carries the injected datetime block.
    assert!(!s.turns[0].text.contains("<current_datetime>"));
    let mirror = render_mirror(&s).content;
    assert!(
        !mirror.contains("You are the GitHub Copilot CLI"),
        "system prompt leaked"
    );
    assert!(
        !mirror.contains("environment_limitations"),
        "instruction.update leaked"
    );
    // Line-based traces return the turn's own record plus `--context` lines; the tool events
    // of this turn sit on the 5 lines before the final assistant message.
    let raw = extract_raw_trace(
        AgentKind::Copilot,
        &s.source_path,
        &s.turns[3].source_offset,
        5,
    )
    .unwrap();
    assert!(
        raw.contains("tool.execution_start") && raw.contains("TOOLRESULT_MARKER"),
        "{raw}"
    );
}

#[test]
fn copilot_skips_empty_assistant_ephemeral_cwd_and_reads_events_without_workspace() {
    let dir = TempDir::new().unwrap();
    let ev = |ty: &str, data: serde_json::Value| json!({"type": ty, "id": ty, "timestamp": "2026-11-13T00:00:00.000Z", "data": data});
    let sd = copilot_session_dir(dir.path(), "cp-1");
    write_jsonl_lines(
        &sd.join("events.jsonl"),
        &[
            ev(
                "session.start",
                json!({"sessionId": "cp-1", "copilotVersion": "1.0.110", "context": {"cwd": "/Users/a/proj"}}),
            ),
            ev(
                "user.message",
                json!({"content": "copilot_q1", "transformedContent": "<current_datetime>x</current_datetime> copilot_q1"}),
            ),
            ev(
                "assistant.message",
                json!({"content": "", "toolRequests": [{"toolCallId": "c", "name": "bash", "arguments": {"command": "COPILOT_TOOLCALL_NOISE"}}]}),
            ),
            ev(
                "tool.execution_complete",
                json!({"toolCallId": "c", "success": true, "result": {"content": "COPILOT_TOOLRESULT_NOISE"}}),
            ),
            ev(
                "assistant.message",
                json!({"content": "copilot_a1", "reasoningText": "COPILOT_REASONING_NOISE", "toolRequests": []}),
            ),
            ev("system.message", json!({"content": "COPILOT_SYSTEM_NOISE"})),
        ],
    );
    let sd2 = copilot_session_dir(dir.path(), "cp-tmp");
    write_jsonl_lines(
        &sd2.join("events.jsonl"),
        &[
            ev(
                "session.start",
                json!({"sessionId": "cp-tmp", "context": {"cwd": "/tmp/scratch"}}),
            ),
            ev("user.message", json!({"content": "tmp question"})),
            ev("assistant.message", json!({"content": "tmp answer"})),
        ],
    );
    let roots = SourceRoots {
        copilot: vec![dir.path().to_path_buf()],
        ..Default::default()
    };
    let opts = ParseOptions::default();
    let sessions: Vec<_> = discover_all(&roots, None, &opts)
        .unwrap()
        .iter()
        .filter_map(|c| match parse_candidate(c, &opts).unwrap() {
            ParseOutcome::Session(s) => Some(s),
            ParseOutcome::Skipped { .. } => None,
        })
        .collect();
    assert_eq!(sessions.len(), 1, "tmp-cwd session must be skipped");
    let s = &sessions[0];
    assert_eq!(s.session_id, "cp-1");
    assert_eq!(s.cwd.as_deref(), Some("/Users/a/proj"));
    let got: Vec<_> = s.turns.iter().map(|t| (t.role, t.text.as_str())).collect();
    assert_eq!(
        got,
        [(Role::User, "copilot_q1"), (Role::Assistant, "copilot_a1")]
    );
    let mirror = render_mirror(s).content;
    for n in [
        "COPILOT_TOOLCALL_NOISE",
        "COPILOT_TOOLRESULT_NOISE",
        "COPILOT_REASONING_NOISE",
        "COPILOT_SYSTEM_NOISE",
    ] {
        assert!(!mirror.contains(n), "{n} leaked");
    }
}

#[test]
fn copilot_roots_resolve_from_home_session_state_or_session_dir() {
    let dir = TempDir::new().unwrap();
    let sd = copilot_session_dir(dir.path(), "cp-root");
    write_jsonl_lines(
        &sd.join("events.jsonl"),
        &[
            json!({"type": "session.start", "id": "1", "timestamp": "2026-11-13T00:00:00Z", "data": {"sessionId": "cp-root", "context": {"cwd": "/Users/a/p"}}}),
            json!({"type": "user.message", "id": "2", "timestamp": "2026-11-13T00:00:01Z", "data": {"content": "hi"}}),
        ],
    );
    for root in [
        dir.path().to_path_buf(),
        dir.path().join("session-state"),
        sd.clone(),
    ] {
        let roots = SourceRoots {
            copilot: vec![root.clone()],
            ..Default::default()
        };
        assert_eq!(
            discover_all(&roots, None, &ParseOptions::default())
                .unwrap()
                .len(),
            1,
            "root {}",
            root.display()
        );
    }
}

#[test]
fn continue_real_recorded_session_keeps_user_and_assistant_text_only() {
    // Files copied verbatim from a session the real Continue CLI 1.5.47 wrote, including a real
    // tool-calling turn (assistant message with empty content + `toolCalls`).
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let fx = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let id = "d3f600b1-a62c-4aca-a01d-c811cd223dc6";
    std::fs::copy(
        fx.join("continue_real_session.json"),
        sessions.join(format!("{id}.json")),
    )
    .unwrap();
    std::fs::copy(
        fx.join("continue_real_sessions_index.json"),
        sessions.join("sessions.json"),
    )
    .unwrap();
    let roots = SourceRoots {
        continue_cli: vec![dir.path().to_path_buf()],
        ..Default::default()
    };
    let opts = ParseOptions {
        include_ephemeral: true, // fixture workspace is under /private/tmp
        ..Default::default()
    };
    let cands = discover_all(&roots, None, &opts).unwrap();
    assert_eq!(
        cands.len(),
        1,
        "sessions.json index must not be treated as a session"
    );
    let ParseOutcome::Session(s) = parse_candidate(&cands[0], &opts).unwrap() else {
        panic!("expected continue session");
    };
    assert_eq!(s.agent, AgentKind::Continue);
    assert_eq!(s.session_id, id);
    assert_eq!(s.cwd.as_deref(), Some("/private/tmp/qa-home-cn/proj"));
    assert_eq!(s.started_at.as_deref(), Some("2026-11-13T13:52:56.115Z"));
    let got: Vec<_> = s.turns.iter().map(|t| (t.role, t.text.as_str())).collect();
    assert_eq!(got.len(), 6, "{got:?}"); // tool-only assistant message is dropped
    assert_eq!(got[0], (Role::User, "qa_cn_alpha 라는 단어를 설명해줘"));
    assert_eq!(got[4], (Role::User, "TOOLTEST qa_cn_gamma run echo"));
    assert!(got[5].1.starts_with("MOCKREPLY"));
    let mirror = render_mirror(&s).content;
    assert!(
        !mirror.contains("TOOLRESULT_MARKER"),
        "tool call arguments leaked"
    );
    // Trace of an assistant turn returns the whole history entry, tool calls included.
    let raw = extract_raw_trace(
        AgentKind::Continue,
        &s.source_path,
        &s.turns[5].source_offset,
        0,
    )
    .unwrap();
    assert!(raw.contains("MOCKREPLY"), "{raw}");
    let raw_prev = extract_raw_trace(
        AgentKind::Continue,
        &s.source_path,
        &SourceOffset::Item {
            item_id: "5".into(),
        },
        0,
    )
    .unwrap();
    assert!(raw_prev.contains("TOOLRESULT_MARKER"), "{raw_prev}");
}

#[test]
fn continue_skips_empty_and_ephemeral_sessions_and_handles_array_content() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let h = |role: &str, content: serde_json::Value| json!({"message": {"role": role, "content": content}, "contextItems": []});
    std::fs::write(
        sessions.join("s1.json"),
        json!({"sessionId": "s1", "title": "chat one", "workspaceDirectory": "/Users/a/proj", "history": [
            h("user", json!("continue_q1")),
            h("assistant", json!([{"type": "text", "text": "continue_a1"}, {"type": "imageUrl", "imageUrl": {"url": "CONTINUE_IMAGE_NOISE"}}])),
            h("system", json!("CONTINUE_SYSTEM_NOISE")),
            h("tool", json!("CONTINUE_TOOLRESULT_NOISE")),
            h("assistant", json!("")),
        ]}).to_string(),
    ).unwrap();
    std::fs::write(
        sessions.join("empty.json"),
        json!({"sessionId": "empty", "workspaceDirectory": "/Users/a/p", "history": []})
            .to_string(),
    )
    .unwrap();
    std::fs::write(sessions.join("tmp.json"), json!({"sessionId": "tmp", "workspaceDirectory": "/tmp/scratch", "history": [h("user", json!("tmp q"))]}).to_string()).unwrap();
    let roots = SourceRoots {
        continue_cli: vec![dir.path().to_path_buf()],
        ..Default::default()
    };
    let opts = ParseOptions::default();
    let sessions: Vec<_> = discover_all(&roots, None, &opts)
        .unwrap()
        .iter()
        .filter_map(|c| match parse_candidate(c, &opts).unwrap() {
            ParseOutcome::Session(s) => Some(s),
            ParseOutcome::Skipped { .. } => None,
        })
        .collect();
    assert_eq!(sessions.len(), 1);
    let s = &sessions[0];
    assert_eq!(s.title.as_deref(), Some("chat one"));
    let got: Vec<_> = s.turns.iter().map(|t| (t.role, t.text.as_str())).collect();
    assert_eq!(
        got,
        [
            (Role::User, "continue_q1"),
            (Role::Assistant, "continue_a1")
        ]
    );
    let mirror = render_mirror(s).content;
    for n in [
        "CONTINUE_IMAGE_NOISE",
        "CONTINUE_SYSTEM_NOISE",
        "CONTINUE_TOOLRESULT_NOISE",
    ] {
        assert!(!mirror.contains(n), "{n} leaked");
    }
}

#[test]
fn continue_roots_accept_home_or_sessions_dir() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::write(
        sessions.join("a.json"),
        json!({"sessionId": "a", "workspaceDirectory": "/Users/a/p", "history": [{"message": {"role": "user", "content": "hi"}}]}).to_string(),
    ).unwrap();
    for root in [dir.path().to_path_buf(), sessions.clone()] {
        let roots = SourceRoots {
            continue_cli: vec![root.clone()],
            ..Default::default()
        };
        assert_eq!(
            discover_all(&roots, None, &ParseOptions::default())
                .unwrap()
                .len(),
            1,
            "root {}",
            root.display()
        );
    }
}
