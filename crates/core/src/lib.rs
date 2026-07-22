use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use globset::{Glob, GlobSet, GlobSetBuilder};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
};
use uuid::Uuid;
use walkdir::WalkDir;

pub const SCHEMA_VERSION: &str = "1.0";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Succeeded,
    Failed,
    Interrupted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunManifest {
    pub schema_version: String,
    pub run_id: String,
    pub project: String,
    pub command: Vec<String>,
    pub cwd: String,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub status: RunStatus,
    pub exit_code: Option<i32>,
    pub event_count: u64,
    pub redaction_count: u64,
}

impl RunManifest {
    pub fn new(project: String, command: Vec<String>, cwd: &Path) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.into(),
            run_id: format!("run_{}", Uuid::now_v7().simple()),
            project,
            command,
            cwd: cwd.display().to_string(),
            started_at: Utc::now(),
            ended_at: None,
            status: RunStatus::Running,
            exit_code: None,
            event_count: 0,
            redaction_count: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Actor {
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub schema_version: String,
    pub run_id: String,
    pub sequence: u64,
    pub timestamp: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span_id: Option<String>,
    pub actor: Actor,
    pub event_type: String,
    pub payload: Value,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifact_refs: Vec<String>,
}

impl Event {
    pub fn new(run_id: &str, sequence: u64, event_type: impl Into<String>, payload: Value) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.into(),
            run_id: run_id.into(),
            sequence,
            timestamp: Utc::now(),
            span_id: None,
            actor: Actor {
                kind: "system".into(),
                name: "process-capture".into(),
            },
            event_type: event_type.into(),
            payload,
            artifact_refs: vec![],
        }
    }
}

pub struct Redactor {
    rules: Vec<(String, Regex, String)>,
    env_values: Vec<String>,
}

impl Redactor {
    pub fn standard() -> Self {
        let specs = [
            (
                "openai-key",
                r"sk-[A-Za-z0-9_-]{16,}",
                "<redacted:openai-key>",
            ),
            (
                "github-token",
                r"gh[pousr]_[A-Za-z0-9]{20,}",
                "<redacted:github-token>",
            ),
            (
                "authorization",
                r"(?i)Bearer\s+[A-Za-z0-9._~+/=-]{12,}",
                "Bearer <redacted:token>",
            ),
            (
                "private-key",
                r"-----BEGIN [A-Z ]*PRIVATE KEY-----",
                "<redacted:private-key>",
            ),
        ];
        let rules = specs
            .into_iter()
            .map(|(n, p, r)| (n.into(), Regex::new(p).unwrap(), r.into()))
            .collect();
        let env_values = env::vars()
            .filter(|(k, v)| is_secret_env(k) && v.len() >= 8)
            .map(|(_, v)| v)
            .collect();
        Self { rules, env_values }
    }

    pub fn redact(&self, input: &str) -> (String, u64) {
        let mut value = input.to_string();
        let mut count = 0;
        for secret in &self.env_values {
            let hits = value.matches(secret).count() as u64;
            if hits > 0 {
                value = value.replace(secret, "<redacted:env>");
                count += hits;
            }
        }
        for (_, pattern, replacement) in &self.rules {
            let hits = pattern.find_iter(&value).count() as u64;
            if hits > 0 {
                value = pattern
                    .replace_all(&value, replacement.as_str())
                    .into_owned();
                count += hits;
            }
        }
        (value, count)
    }
}

fn is_secret_env(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    ["TOKEN", "SECRET", "PASSWORD", "API_KEY", "PRIVATE_KEY"]
        .iter()
        .any(|part| upper.contains(part))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileState {
    pub hash: String,
    pub size: u64,
}
pub type WorkspaceSnapshot = BTreeMap<String, FileState>;

pub fn snapshot(root: &Path) -> Result<WorkspaceSnapshot> {
    let excludes = build_excludes()?;
    let storage_root = data_home().ok().filter(|path| path.starts_with(root));
    let mut result = BTreeMap::new();
    for entry in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }
        if storage_root
            .as_ref()
            .is_some_and(|storage| entry.path().starts_with(storage))
        {
            continue;
        }
        let relative = entry.path().strip_prefix(root).unwrap();
        if excludes.is_match(relative) {
            continue;
        }
        let bytes =
            fs::read(entry.path()).with_context(|| format!("read {}", entry.path().display()))?;
        result.insert(
            relative.to_string_lossy().replace('\\', "/"),
            FileState {
                hash: blake3::hash(&bytes).to_hex().to_string(),
                size: bytes.len() as u64,
            },
        );
    }
    Ok(result)
}

fn build_excludes() -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in [
        ".git/**",
        ".agentflight/**",
        "target/**",
        "node_modules/**",
        "*.afrun",
    ] {
        builder.add(Glob::new(pattern)?);
    }
    Ok(builder.build()?)
}

pub fn file_change_events(
    run_id: &str,
    start_sequence: u64,
    before: &WorkspaceSnapshot,
    after: &WorkspaceSnapshot,
) -> Vec<Event> {
    let mut events = vec![];
    let mut sequence = start_sequence;
    for (path, state) in after {
        let change = match before.get(path) {
            None => Some("added"),
            Some(old) if old.hash != state.hash => Some("modified"),
            _ => None,
        };
        if let Some(change) = change {
            events.push(Event::new(
                run_id,
                sequence,
                "file.change",
                json!({"path": path, "change": change, "hash": state.hash, "size": state.size}),
            ));
            sequence += 1;
        }
    }
    for (path, state) in before {
        if !after.contains_key(path) {
            events.push(Event::new(run_id, sequence, "file.change", json!({"path": path, "change": "deleted", "previous_hash": state.hash, "size": state.size})));
            sequence += 1;
        }
    }
    events
}

pub fn redact_json(redactor: &Redactor, value: Value) -> (Value, u64) {
    match value {
        Value::String(s) => {
            let (v, n) = redactor.redact(&s);
            (Value::String(v), n)
        }
        Value::Array(values) => {
            let mut n = 0;
            let values = values
                .into_iter()
                .map(|v| {
                    let (v, c) = redact_json(redactor, v);
                    n += c;
                    v
                })
                .collect();
            (Value::Array(values), n)
        }
        Value::Object(values) => {
            let mut n = 0;
            let values: Map<_, _> = values
                .into_iter()
                .map(|(k, v)| {
                    let (v, c) = redact_json(redactor, v);
                    n += c;
                    (k, v)
                })
                .collect();
            (Value::Object(values), n)
        }
        other => (other, 0),
    }
}

pub fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_vec_pretty(value)?)?;
    Ok(())
}

pub fn append_event(path: &Path, event: &Event) -> Result<()> {
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    serde_json::to_writer(&mut file, event)?;
    writeln!(file)?;
    file.sync_data()?;
    Ok(())
}

pub fn read_events(path: &Path) -> Result<Vec<Event>> {
    let text = fs::read_to_string(path)?;
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).context("invalid event JSON"))
        .collect()
}

pub fn data_home() -> Result<PathBuf> {
    if let Some(path) = env::var_os("AGENTFLIGHT_HOME") {
        return Ok(PathBuf::from(path));
    }
    let home = env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".agentflight"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn redacts_standard_secret() {
        let (out, count) = Redactor::standard().redact("key=sk-abcdefghijklmnopqrstuv");
        assert_eq!(count, 1);
        assert!(!out.contains("abcdefghijkl"));
    }
    #[test]
    fn detects_file_changes() {
        let mut a = WorkspaceSnapshot::new();
        let mut b = WorkspaceSnapshot::new();
        a.insert(
            "old".into(),
            FileState {
                hash: "a".into(),
                size: 1,
            },
        );
        b.insert(
            "new".into(),
            FileState {
                hash: "b".into(),
                size: 2,
            },
        );
        assert_eq!(file_change_events("run", 1, &a, &b).len(), 2);
    }
}
