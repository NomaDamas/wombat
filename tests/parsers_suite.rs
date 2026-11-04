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
        "/Users/jeffrey/.autorag/workspace",
        "/Users/jeffrey/.autorag/workspace/sub",
        "/Volumes/SSD1/4/AutoRAG/.bench/ws-noemit",
        "/Volumes/SSD1/4/AutoRAG/.bench-ws-main",
    ] {
        assert!(is_ephemeral_temp_cwd(c), "{c} should be ephemeral");
    }
    for c in [
        "/Volumes/SSD1/1/wombat",
        "/Users/jeffrey/.autorag-notes",
        "/Users/jeffrey/Projects/autorag/workspace",
        "/Users/jeffrey/Projects/workbench/notes",
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
                "cwd": "/Users/jeffrey/Projects/banana",
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
                    {"type": "input_text", "text": "<environment_context><cwd>/Users/jeffrey/Projects/banana</cwd></environment_context>"}
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
                    "cwd": "/Users/jeffrey/Projects/banana",
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
                "session_id": "exec-1", "cwd": "/Users/jeffrey/Projects/k-skill",
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
