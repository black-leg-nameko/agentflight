use agentflight_core::{Event, Redactor, redact_json};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    fs,
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    thread,
};

pub const CAPTURE_PATH_ENV: &str = "AGENTFLIGHT_MCP_CAPTURE";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureRecord {
    pub timestamp: DateTime<Utc>,
    pub direction: Direction,
    pub message: Value,
    #[serde(default)]
    pub redaction_count: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    ClientToServer,
    ServerToClient,
}

pub fn proxy(command: &[String], capture_path: Option<&Path>) -> Result<i32> {
    if command.is_empty() {
        bail!("MCP server command cannot be empty");
    }
    let mut child = Command::new(&command[0])
        .args(&command[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("start MCP server {}", command[0]))?;

    let capture = capture_path
        .map(|path| -> Result<_> {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            Ok(Arc::new(Mutex::new(
                fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)?,
            )))
        })
        .transpose()?;

    let mut server_stdin = child.stdin.take().context("MCP server stdin unavailable")?;
    let client_capture = capture.clone();
    thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            if record_line(client_capture.as_ref(), Direction::ClientToServer, &line).is_err() {
                break;
            }
            if writeln!(server_stdin, "{line}")
                .and_then(|_| server_stdin.flush())
                .is_err()
            {
                break;
            }
        }
    });

    let stderr = child
        .stderr
        .take()
        .context("MCP server stderr unavailable")?;
    let stderr_thread = thread::spawn(move || {
        let mut reader = stderr;
        let _ = std::io::copy(&mut reader, &mut std::io::stderr());
    });

    let stdout = child
        .stdout
        .take()
        .context("MCP server stdout unavailable")?;
    let mut client_stdout = std::io::stdout().lock();
    for line in BufReader::new(stdout).lines() {
        let line = line?;
        record_line(capture.as_ref(), Direction::ServerToClient, &line)?;
        writeln!(client_stdout, "{line}")?;
        client_stdout.flush()?;
    }
    let status = child.wait()?;
    let _ = stderr_thread.join();
    Ok(status.code().unwrap_or(1))
}

fn record_line(
    capture: Option<&Arc<Mutex<fs::File>>>,
    direction: Direction,
    line: &str,
) -> Result<()> {
    let Some(capture) = capture else {
        return Ok(());
    };
    let message: Value = match serde_json::from_str(line) {
        Ok(message) => message,
        Err(_) => return Ok(()),
    };
    let (message, redaction_count) = redact_json(&Redactor::standard(), message);
    let record = CaptureRecord {
        timestamp: Utc::now(),
        direction,
        message,
        redaction_count,
    };
    let mut file = capture
        .lock()
        .map_err(|_| anyhow::anyhow!("MCP capture lock poisoned"))?;
    serde_json::to_writer(&mut *file, &record)?;
    writeln!(file)?;
    file.sync_data()?;
    Ok(())
}

pub fn read_capture(path: &Path) -> Result<Vec<CaptureRecord>> {
    let input = fs::read_to_string(path)?;
    input
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).context("invalid MCP capture record"))
        .collect()
}

pub fn normalize(run_id: &str, start_sequence: u64, records: Vec<CaptureRecord>) -> Vec<Event> {
    let mut pending = HashMap::<String, String>::new();
    let mut sequence = start_sequence;
    records
        .into_iter()
        .map(|record| {
            let id = record.message.get("id").map(canonical_id);
            let method = record
                .message
                .get("method")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let event_type = match (record.direction, method.as_deref()) {
                (Direction::ClientToServer, Some("initialize")) => "mcp.initialize",
                (Direction::ClientToServer, Some("tools/list")) => "mcp.tools.list",
                (Direction::ClientToServer, Some("tools/call")) => "mcp.tool.call",
                (Direction::ClientToServer, Some(_)) => "mcp.request",
                (Direction::ClientToServer, None) => "mcp.client.response",
                (Direction::ServerToClient, Some(_)) => "mcp.notification",
                (Direction::ServerToClient, None) => response_event_type(
                    &record.message,
                    id.as_ref()
                        .and_then(|id| pending.get(id))
                        .map(String::as_str),
                ),
            };
            if matches!(record.direction, Direction::ClientToServer)
                && let (Some(id), Some(method)) = (&id, method)
            {
                pending.insert(id.clone(), method);
            }
            if matches!(record.direction, Direction::ServerToClient)
                && let Some(id) = &id
            {
                pending.remove(id);
            }
            let mut event = Event::new(
                run_id,
                sequence,
                event_type,
                json!({
                    "direction": record.direction,
                    "message": record.message
                }),
            );
            event.timestamp = record.timestamp;
            event.actor.kind = if matches!(record.direction, Direction::ClientToServer) {
                "agent".into()
            } else {
                "tool".into()
            };
            event.actor.name = "mcp".into();
            sequence += 1;
            event
        })
        .collect()
}

fn canonical_id(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".into())
}

fn response_event_type(message: &Value, request_method: Option<&str>) -> &'static str {
    if message.get("error").is_some() {
        return if request_method == Some("tools/call") {
            "mcp.tool.error"
        } else {
            "mcp.error"
        };
    }
    match request_method {
        Some("tools/call") => "mcp.tool.result",
        Some("tools/list") => "mcp.tools.list.result",
        Some("initialize") => "mcp.initialize.result",
        _ => "mcp.response",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn correlates_tool_calls_and_results() {
        let now = Utc::now();
        let records = vec![
            CaptureRecord {
                timestamp: now,
                direction: Direction::ClientToServer,
                message: json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"read_file"}}),
                redaction_count: 0,
            },
            CaptureRecord {
                timestamp: now,
                direction: Direction::ServerToClient,
                message: json!({"jsonrpc":"2.0","id":1,"result":{"content":[]}}),
                redaction_count: 0,
            },
        ];
        let events = normalize("run_test", 8, records);
        assert_eq!(events[0].event_type, "mcp.tool.call");
        assert_eq!(events[1].event_type, "mcp.tool.result");
        assert_eq!(events[1].sequence, 9);
    }

    #[test]
    fn redacts_messages_before_capture() -> Result<()> {
        let temp = tempfile::NamedTempFile::new()?;
        let file = fs::OpenOptions::new().append(true).open(temp.path())?;
        let capture = Arc::new(Mutex::new(file));
        record_line(
            Some(&capture),
            Direction::ClientToServer,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"token":"sk-abcdefghijklmnopqrstuv"}}"#,
        )?;
        drop(capture);
        let records = read_capture(temp.path())?;
        assert_eq!(records[0].redaction_count, 1);
        assert!(!records[0].message.to_string().contains("abcdefghijkl"));
        Ok(())
    }
}
