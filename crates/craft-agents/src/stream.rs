//! Turns coding-agent NDJSON stream events into short, readable log lines.

use crate::CodingAgentKind;
use craft_core::LogLevel;
use serde_json::Value;

const MAX_LEN: usize = 400;

fn clip(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > MAX_LEN {
        format!("{}…", flat.chars().take(MAX_LEN).collect::<String>())
    } else {
        flat
    }
}

fn text_of<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| v.get(k)?.as_str())
}

/// Returns the log entries for one line of agent stdout. Plain-text lines pass through unchanged.
pub fn summarize_line(kind: &CodingAgentKind, line: &str) -> Vec<(LogLevel, String)> {
    let line = strip_ansi(line);
    let line = line.trim();
    if line.is_empty() {
        return Vec::new();
    }
    match serde_json::from_str::<Value>(line) {
        Ok(v) if v.is_object() => match kind {
            CodingAgentKind::Codex => codex_event(&v),
            CodingAgentKind::Agy => agy_event(&v),
            _ => claude_event(&v),
        },
        _ => vec![(LogLevel::Info, clip(line))],
    }
}

/// Claude Code `--output-format stream-json`; `agy` and unknown agents fall through to `generic`.
fn claude_event(v: &Value) -> Vec<(LogLevel, String)> {
    let blocks = |v: &Value| {
        v.pointer("/message/content")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    match v.get("type").and_then(Value::as_str) {
        Some("system") => match text_of(v, &["subtype"]) {
            Some("init") => vec![(
                LogLevel::Info,
                format!(
                    "Agent session started ({})",
                    text_of(v, &["model"]).unwrap_or("default model")
                ),
            )],
            _ => Vec::new(),
        },
        Some("assistant") => blocks(v)
            .iter()
            .filter_map(|b| match b.get("type").and_then(Value::as_str) {
                Some("text") => Some((LogLevel::Info, clip(text_of(b, &["text"])?))),
                Some("tool_use") => {
                    let input = b.get("input").cloned().unwrap_or_default();
                    let detail = text_of(
                        &input,
                        &["file_path", "command", "pattern", "path", "url", "description"],
                    )
                    .unwrap_or("");
                    Some((
                        LogLevel::Info,
                        clip(&format!("→ {} {detail}", text_of(b, &["name"]).unwrap_or("tool"))),
                    ))
                }
                _ => None,
            })
            .collect(),
        Some("user") => blocks(v)
            .iter()
            .filter(|b| b.get("is_error").and_then(Value::as_bool) == Some(true))
            .map(|b| {
                let detail = match b.get("content") {
                    Some(Value::String(s)) => s.clone(),
                    Some(other) => other.to_string(),
                    None => String::new(),
                };
                (LogLevel::Warn, clip(&format!("Tool error: {detail}")))
            })
            .collect(),
        Some("result") => {
            let failed = v.get("is_error").and_then(Value::as_bool) == Some(true);
            vec![(
                if failed { LogLevel::Error } else { LogLevel::Info },
                clip(&format!(
                    "Agent finished: {}",
                    text_of(v, &["result", "subtype"]).unwrap_or("done")
                )),
            )]
        }
        Some("stream_event") => Vec::new(),
        _ => generic(v),
    }
}

/// Codex CLI `exec --json` thread/turn/item events.
fn codex_event(v: &Value) -> Vec<(LogLevel, String)> {
    let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
    let item = v.get("item").cloned().unwrap_or_default();
    let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
    let one = |level, text: String| vec![(level, clip(&text))];
    match (kind, item_type) {
        ("thread.started", _) => one(LogLevel::Info, "Agent session started".into()),
        ("turn.completed", _) => one(
            LogLevel::Info,
            format!(
                "Agent turn completed ({} output tokens)",
                v.pointer("/usage/output_tokens").and_then(Value::as_u64).unwrap_or(0)
            ),
        ),
        ("turn.failed", _) => one(
            LogLevel::Error,
            format!(
                "Agent turn failed: {}",
                v.pointer("/error/message").and_then(Value::as_str).unwrap_or("unknown error")
            ),
        ),
        ("error", _) => one(
            LogLevel::Error,
            text_of(v, &["message"]).unwrap_or("agent error").into(),
        ),
        ("item.started", "command_execution") => one(
            LogLevel::Info,
            format!("$ {}", text_of(&item, &["command"]).unwrap_or("")),
        ),
        ("item.completed", "command_execution") => {
            match item.get("exit_code").and_then(Value::as_i64) {
                Some(code) if code != 0 => one(
                    LogLevel::Warn,
                    format!("exit {code}: {}", text_of(&item, &["command"]).unwrap_or("")),
                ),
                _ => Vec::new(),
            }
        }
        ("item.completed", "agent_message") => one(
            LogLevel::Info,
            text_of(&item, &["text"]).unwrap_or("").into(),
        ),
        ("item.completed", "reasoning") => one(
            LogLevel::Debug,
            text_of(&item, &["text"]).unwrap_or("").into(),
        ),
        ("item.completed", "file_change") => item
            .get("changes")
            .and_then(Value::as_array)
            .map(|changes| {
                changes
                    .iter()
                    .map(|c| {
                        (
                            LogLevel::Info,
                            clip(&format!(
                                "✎ {} {}",
                                text_of(c, &["kind"]).unwrap_or("edit"),
                                text_of(c, &["path"]).unwrap_or("")
                            )),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default(),
        ("item.completed", "error") => one(
            LogLevel::Error,
            text_of(&item, &["message"]).unwrap_or("agent error").into(),
        ),
        ("turn.started", _) | ("item.started", _) | ("item.updated", _) => Vec::new(),
        ("item.completed", _) => one(LogLevel::Debug, format!("{item_type} completed")),
        _ => generic(v),
    }
}

/// Antigravity CLI `--output-format stream-json`: init, step_update and result events.
fn agy_event(v: &Value) -> Vec<(LogLevel, String)> {
    let step = &v["step_update"];
    match v.get("event").and_then(Value::as_str) {
        Some("init") => vec![(LogLevel::Info, "Agent session started".into())],
        Some("step_update") if step["step_type"] == "tool" => {
            // The first string parameter is the file, command or query the tool acts on.
            let detail = step["tool_info"]["parameters"]
                .as_object()
                .and_then(|p| p.values().find_map(Value::as_str))
                .unwrap_or("");
            let name = text_of(step, &["tool_name"]).unwrap_or("tool");
            match text_of(step, &["state"]) {
                Some("ACTIVE") => vec![(LogLevel::Info, clip(&format!("→ {name} {detail}")))],
                Some("DONE") | None => Vec::new(),
                Some(other) => vec![(
                    LogLevel::Warn,
                    clip(&format!("{name} ended as {other}: {detail}")),
                )],
            }
        }
        // Reply text arrives as small deltas; the final result event carries it whole.
        Some("step_update") => Vec::new(),
        Some("result") => {
            let result = &v["result"];
            let failed = result["status"] != "SUCCESS";
            vec![(
                if failed { LogLevel::Error } else { LogLevel::Info },
                clip(&format!(
                    "Agent finished: {}",
                    text_of(result, &["response", "status"]).unwrap_or("done")
                )),
            )]
        }
        _ => generic(v),
    }
}

/// Removes terminal colour and cursor escape sequences from a line of agent output.
pub fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // CSI sequence: ESC [ parameters, ended by a letter.
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn generic(v: &Value) -> Vec<(LogLevel, String)> {
    match text_of(v, &["message", "text", "content", "result"]) {
        Some(text) => vec![(LogLevel::Info, clip(text))],
        None => vec![(
            LogLevel::Debug,
            clip(&format!("event: {}", text_of(v, &["type"]).unwrap_or("unknown"))),
        )],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn messages(kind: CodingAgentKind, line: &str) -> Vec<String> {
        summarize_line(&kind, line).into_iter().map(|(_, m)| m).collect()
    }

    #[test]
    fn claude_stream_json_becomes_readable_lines() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Creating the page"},{"type":"tool_use","name":"Write","input":{"file_path":"index.html","content":"<h1>x</h1>"}}]}}"#;
        assert_eq!(
            messages(CodingAgentKind::Claude, line),
            vec!["Creating the page", "→ Write index.html"]
        );
        let result = r#"{"type":"result","subtype":"success","is_error":false,"result":"All done"}"#;
        assert_eq!(messages(CodingAgentKind::Claude, result), vec!["Agent finished: All done"]);
        let init = r#"{"type":"system","subtype":"init","model":"claude-x"}"#;
        assert_eq!(messages(CodingAgentKind::Claude, init), vec!["Agent session started (claude-x)"]);
    }

    #[test]
    fn codex_json_events_become_readable_lines() {
        let cmd = r#"{"type":"item.started","item":{"id":"i1","type":"command_execution","command":"ls -la"}}"#;
        assert_eq!(messages(CodingAgentKind::Codex, cmd), vec!["$ ls -la"]);
        let change = r#"{"type":"item.completed","item":{"type":"file_change","changes":[{"path":"a.rs","kind":"add"}]}}"#;
        assert_eq!(messages(CodingAgentKind::Codex, change), vec!["✎ add a.rs"]);
        let failed = r#"{"type":"turn.failed","error":{"message":"quota"}}"#;
        assert_eq!(
            summarize_line(&CodingAgentKind::Codex, failed),
            vec![(LogLevel::Error, "Agent turn failed: quota".to_string())]
        );
    }

    #[test]
    fn agy_stream_json_becomes_readable_lines() {
        let tool = r#"{"event":"step_update","step_update":{"state":"ACTIVE","step_type":"tool","tool_name":"write_to_file","tool_info":{"name":"write_to_file","parameters":{"Path":"index.html"}}}}"#;
        assert_eq!(messages(CodingAgentKind::Agy, tool), vec!["→ write_to_file index.html"]);
        let done = tool.replace("ACTIVE", "DONE");
        assert!(messages(CodingAgentKind::Agy, &done).is_empty());
        let delta = r#"{"event":"step_update","step_update":{"state":"ACTIVE","step_type":"agent_response","text_delta":"Recre"}}"#;
        assert!(messages(CodingAgentKind::Agy, delta).is_empty());
        let result = r#"{"event":"result","result":{"status":"SUCCESS","response":"Built the page"}}"#;
        assert_eq!(messages(CodingAgentKind::Agy, result), vec!["Agent finished: Built the page"]);
        let failed = r#"{"event":"result","result":{"status":"ERROR"}}"#;
        assert_eq!(
            summarize_line(&CodingAgentKind::Agy, failed),
            vec![(LogLevel::Error, "Agent finished: ERROR".to_string())]
        );
    }

    #[test]
    fn terminal_escape_codes_are_removed() {
        assert_eq!(strip_ansi("\u{1b}[0m\u{1b}[91m\u{1b}[1mError: \u{1b}[0mFile not found"), "Error: File not found");
        assert_eq!(messages(CodingAgentKind::OpenCode, "\u{1b}[0m← \u{1b}[0mWrite index.html"), vec!["← Write index.html"]);
        assert!(summarize_line(&CodingAgentKind::OpenCode, "\u{1b}[0m").is_empty());
    }

    #[test]
    fn plain_text_and_unknown_json_do_not_get_lost() {
        assert_eq!(messages(CodingAgentKind::OpenCode, "building…"), vec!["building…"]);
        assert_eq!(messages(CodingAgentKind::Gemini, r#"{"type":"note","text":"hello"}"#), vec!["hello"]);
        assert!(summarize_line(&CodingAgentKind::Claude, "   ").is_empty());
    }
}
