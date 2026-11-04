use crate::model::{AgentKind, ParsedSession, Role, SourceOffset};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MirrorTurnSpan {
    /// 1-based turn index within the rendered session mirror.
    pub turn_index: usize,
    pub role: Role,
    pub timestamp: Option<String>,
    pub source_offset: SourceOffset,
    /// Byte span of the entire turn section (`## Turn N ...` through trailing blank line).
    pub mirror_start_byte: usize,
    pub mirror_end_byte: usize,
    /// 1-based inclusive line numbers in the mirror file.
    pub mirror_start_line: usize,
    pub mirror_end_line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedMirror {
    /// Relative path inside the mirror workspace root (forward slashes).
    pub relative_path: String,
    pub content: String,
    pub sha256_hex: String,
    pub turn_spans: Vec<MirrorTurnSpan>,
}

/// Deterministic, filesystem-safe relative mirror path for a session:
/// `<agent>/<project_slug>/<safe_session_stem>.md`.
pub fn mirror_relative_path(session: &ParsedSession) -> String {
    let project_slug = session
        .cwd
        .as_deref()
        .map(slug_from_cwd)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "_global".to_string());
    let stem = safe_component(&session.session_id, 96);
    let short_hash = short_sha256(&format!(
        "{}\0{}\0{}",
        session.agent.as_str(),
        session.source_path,
        session.session_id
    ));
    format!(
        "{}/{}/{}__{}.md",
        session.agent.as_str(),
        project_slug,
        stem,
        short_hash
    )
}

/// Render a `ParsedSession` into a prefix-stable Markdown mirror document.
///
/// Invariant: appending new `Turn` entries to `session.turns` while keeping
/// immutable header fields (`agent`, `session_id`, `source_path`, `cwd`,
/// `started_at`) identical produces a strict byte-prefix extension of `content`
/// and leaves all prior `MirrorTurnSpan` entries byte-for-byte unchanged.
pub fn render_mirror(session: &ParsedSession) -> RenderedMirror {
    let relative_path = mirror_relative_path(session);
    let mut out = String::new();
    let mut line_no: usize = 1;

    push_line(
        &mut out,
        &mut line_no,
        &format!(
            "# Session {}:{}",
            session.agent.as_str(),
            sanitize_header_value(&session.session_id)
        ),
    );
    push_line(
        &mut out,
        &mut line_no,
        &format!("- agent: {}", session.agent.as_str()),
    );
    push_line(
        &mut out,
        &mut line_no,
        &format!(
            "- session_id: {}",
            sanitize_header_value(&session.session_id)
        ),
    );
    push_line(
        &mut out,
        &mut line_no,
        &format!(
            "- source_path: {}",
            sanitize_header_value(&session.source_path)
        ),
    );
    if let Some(cwd) = session.cwd.as_deref().filter(|s| !s.trim().is_empty()) {
        push_line(
            &mut out,
            &mut line_no,
            &format!("- cwd: {}", sanitize_header_value(cwd)),
        );
    }
    if let Some(started) = session
        .started_at
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        push_line(
            &mut out,
            &mut line_no,
            &format!("- started_at: {}", sanitize_header_value(started)),
        );
    }
    push_blank(&mut out, &mut line_no);

    let mut turn_spans = Vec::with_capacity(session.turns.len());
    for (idx, turn) in session.turns.iter().enumerate() {
        let turn_index = idx + 1;
        let mirror_start_byte = out.len();
        let mirror_start_line = line_no;

        let mut heading = format!("## Turn {} | {}", turn_index, turn.role.heading_label());
        if let Some(ts) = turn.timestamp.as_deref().filter(|s| !s.trim().is_empty()) {
            heading.push_str(" | ");
            heading.push_str(&sanitize_header_value(ts));
        }
        heading.push_str(" [src:");
        heading.push_str(&turn.source_offset.compact_label());
        heading.push(']');
        push_line(&mut out, &mut line_no, &heading);
        push_blank(&mut out, &mut line_no);

        let normalized = normalize_turn_body(&turn.text);
        for body_line in normalized.split('\n') {
            push_line(&mut out, &mut line_no, &escape_heading_collision(body_line));
        }
        push_blank(&mut out, &mut line_no);

        let mirror_end_byte = out.len();
        let mirror_end_line = line_no.saturating_sub(1);
        turn_spans.push(MirrorTurnSpan {
            turn_index,
            role: turn.role,
            timestamp: turn.timestamp.clone(),
            source_offset: turn.source_offset.clone(),
            mirror_start_byte,
            mirror_end_byte,
            mirror_start_line,
            mirror_end_line,
        });
    }

    let digest = Sha256::digest(out.as_bytes());
    let sha256_hex = hex::encode(digest);

    RenderedMirror {
        relative_path,
        content: out,
        sha256_hex,
        turn_spans,
    }
}

/// Locate which turn span(s) in `mirror_content` overlap a chunk returned by `minsync query`.
///
/// Strategy:
/// 1. Exact substring search of normalized `chunk_text` inside `mirror_content`.
/// 2. Fallback: strip a leading heading-context line (MinSync >= 0.7 `heading_context`) and retry.
/// 3. Fallback: match the longest non-empty line in `chunk_text`.
/// 4. Parse any `## Turn N | ... [src:...]` header inside `chunk_text` itself.
pub fn locate_chunk_turns<'a>(
    mirror_content: &str,
    turn_spans: &'a [MirrorTurnSpan],
    chunk_text: &str,
) -> Vec<&'a MirrorTurnSpan> {
    if turn_spans.is_empty() || chunk_text.trim().is_empty() {
        return Vec::new();
    }

    let needle = normalize_newlines(chunk_text);
    let trimmed = needle.trim();
    if let Some((start, end)) = find_span_bytes(mirror_content, trimmed) {
        let hits = overlapping_spans(turn_spans, start, end);
        if !hits.is_empty() {
            return hits;
        }
    }

    // MinSync 0.7 heading_context prepends `<path> > <heading>\n` as line 1.
    if let Some((_, after_first)) = trimmed.split_once('\n') {
        let after_trimmed = after_first.trim();
        if !after_trimmed.is_empty()
            && let Some((start, end)) = find_span_bytes(mirror_content, after_trimmed)
        {
            let hits = overlapping_spans(turn_spans, start, end);
            if !hits.is_empty() {
                return hits;
            }
        }
    }

    // Parse explicit `## Turn N |` headings inside the chunk.
    let mut parsed_indices = Vec::new();
    for line in trimmed.lines() {
        if let Some(rest) = line.strip_prefix("## Turn ")
            && let Some((num_str, _)) = rest.split_once(" |")
            && let Ok(idx) = num_str.trim().parse::<usize>()
        {
            parsed_indices.push(idx);
        }
    }
    if !parsed_indices.is_empty() {
        let hits: Vec<&MirrorTurnSpan> = turn_spans
            .iter()
            .filter(|s| parsed_indices.contains(&s.turn_index))
            .collect();
        if !hits.is_empty() {
            return hits;
        }
    }

    // Longest distinctive line fallback.
    if let Some(best_line) = trimmed
        .lines()
        .map(str::trim)
        .filter(|l| l.len() >= 12)
        .max_by_key(|l| l.len())
        && let Some((start, end)) = find_span_bytes(mirror_content, best_line)
    {
        return overlapping_spans(turn_spans, start, end);
    }

    Vec::new()
}

/// Parse metadata header + turn spans back out of an existing mirror file on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedMirrorHeader {
    pub agent: Option<AgentKind>,
    pub session_id: Option<String>,
    pub source_path: Option<String>,
    pub cwd: Option<String>,
    pub started_at: Option<String>,
    pub turn_spans: Vec<MirrorTurnSpan>,
}

pub fn parse_mirror_document(content: &str) -> ParsedMirrorHeader {
    let mut agent = None;
    let mut session_id = None;
    let mut source_path = None;
    let mut cwd = None;
    let mut started_at = None;
    let mut turn_spans: Vec<MirrorTurnSpan> = Vec::new();

    let mut byte_pos = 0usize;
    let mut in_header = true;
    let mut active: Option<MirrorTurnSpan> = None;

    for (idx, raw_line) in content.split_inclusive('\n').enumerate() {
        let line_no = idx + 1;
        let line = raw_line.trim_end_matches(['\r', '\n']);

        if in_header {
            if line.starts_with("## Turn ") {
                in_header = false;
            } else if let Some(v) = line.strip_prefix("- agent: ") {
                agent = AgentKind::parse_name(v);
            } else if let Some(v) = line.strip_prefix("- session_id: ") {
                session_id = Some(v.to_string());
            } else if let Some(v) = line.strip_prefix("- source_path: ") {
                source_path = Some(v.to_string());
            } else if let Some(v) = line.strip_prefix("- cwd: ") {
                cwd = Some(v.to_string());
            } else if let Some(v) = line.strip_prefix("- started_at: ") {
                started_at = Some(v.to_string());
            }
        }

        if let Some(parsed_turn) = parse_turn_heading(line, byte_pos, line_no) {
            if let Some(mut prev) = active.take() {
                prev.mirror_end_byte = byte_pos;
                prev.mirror_end_line = line_no.saturating_sub(1);
                turn_spans.push(prev);
            }
            active = Some(parsed_turn);
        }

        byte_pos += raw_line.len();
    }

    if let Some(mut last) = active.take() {
        last.mirror_end_byte = content.len();
        last.mirror_end_line = content.lines().count().max(last.mirror_start_line);
        turn_spans.push(last);
    }

    ParsedMirrorHeader {
        agent,
        session_id,
        source_path,
        cwd,
        started_at,
        turn_spans,
    }
}

fn parse_turn_heading(line: &str, start_byte: usize, start_line: usize) -> Option<MirrorTurnSpan> {
    let rest = line.strip_prefix("## Turn ")?;
    let (body, src_part) = rest.rsplit_once(" [src:")?;
    let src_label = src_part.strip_suffix(']')?;
    let source_offset = SourceOffset::parse_compact_label(src_label)?;

    let mut parts = body.splitn(3, " | ");
    let turn_index: usize = parts.next()?.trim().parse().ok()?;
    let role_str = parts.next()?.trim();
    let role = match role_str {
        "User" => Role::User,
        "Assistant" => Role::Assistant,
        _ => return None,
    };
    let timestamp = parts
        .next()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    Some(MirrorTurnSpan {
        turn_index,
        role,
        timestamp,
        source_offset,
        mirror_start_byte: start_byte,
        mirror_end_byte: start_byte,
        mirror_start_line: start_line,
        mirror_end_line: start_line,
    })
}

fn find_span_bytes(haystack: &str, needle: &str) -> Option<(usize, usize)> {
    if needle.is_empty() {
        return None;
    }
    let pos = haystack.find(needle)?;
    Some((pos, pos + needle.len()))
}

fn overlapping_spans(spans: &[MirrorTurnSpan], start: usize, end: usize) -> Vec<&MirrorTurnSpan> {
    let hits: Vec<&MirrorTurnSpan> = spans
        .iter()
        .filter(|s| s.mirror_start_byte < end && s.mirror_end_byte > start)
        .collect();
    if !hits.is_empty() {
        return hits;
    }
    // Chunk landed entirely in the file header before Turn 1 -> attach Turn 1.
    if end <= spans[0].mirror_start_byte {
        return vec![&spans[0]];
    }
    Vec::new()
}

fn push_line(buf: &mut String, line_no: &mut usize, line: &str) {
    buf.push_str(line.trim_end_matches([' ', '\t', '\r']));
    buf.push('\n');
    *line_no += 1;
}

fn push_blank(buf: &mut String, line_no: &mut usize) {
    buf.push('\n');
    *line_no += 1;
}

fn normalize_newlines(raw: &str) -> String {
    raw.replace("\r\n", "\n").replace('\r', "\n")
}

fn normalize_turn_body(raw: &str) -> String {
    let normalized = normalize_newlines(raw);
    normalized.trim_matches('\n').trim().to_string()
}

/// Prevent literal `## Turn ` lines inside user/assistant messages from spoofing turn headers.
fn escape_heading_collision(line: &str) -> String {
    if line.starts_with("## Turn ") || line.starts_with("# Session ") {
        format!(" {line}")
    } else {
        line.to_string()
    }
}

fn sanitize_header_value(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c == '\n' || c == '\r' || c.is_control() {
                ' '
            } else {
                c
            }
        })
        .collect::<String>()
        .trim()
        .to_string()
}

fn slug_from_cwd(cwd: &str) -> String {
    let trimmed = cwd.trim().trim_end_matches(['/', '\\']);
    if trimmed.is_empty() {
        return "_global".to_string();
    }
    let tail: Vec<&str> = trimmed
        .split(['/', '\\'])
        .filter(|seg| !seg.is_empty())
        .collect();
    let chosen = if tail.len() >= 2 {
        format!("{}-{}", tail[tail.len() - 2], tail[tail.len() - 1])
    } else if let Some(last) = tail.last() {
        (*last).to_string()
    } else {
        "_global".to_string()
    };
    safe_component(&chosen, 64)
}

pub fn safe_component(raw: &str, max_len: usize) -> String {
    let mut out = String::with_capacity(raw.len().min(max_len));
    let mut prev_dash = false;
    for ch in raw.chars() {
        if out.len() >= max_len {
            break;
        }
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '.' {
            out.push(ch);
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let trimmed = out.trim_matches(['-', '.']).to_string();
    if trimmed.is_empty() {
        "session".to_string()
    } else {
        trimmed
    }
}

pub fn short_sha256(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    hex::encode(&digest[..6])
}
