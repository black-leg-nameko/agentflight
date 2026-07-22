use agentflight_bundle::{export_run, import_run};
use agentflight_capture_process::run_pty;
use agentflight_core::{
    Event, Redactor, RunManifest, RunStatus, append_event, data_home, file_change_events,
    read_events, redact_json, snapshot, write_json,
};
use agentflight_storage::{ArtifactStore, MetadataStore, RunJournal};
use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Parser)]
#[command(
    name = "agentflight",
    version,
    about = "Flight recorder and regression testing for AI agents"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Init,
    Record {
        #[arg(required = true, last = true)]
        command: Vec<String>,
    },
    List,
    Inspect {
        run_id: String,
        #[arg(long)]
        events: bool,
    },
    Promote {
        run_id: String,
        #[arg(long)]
        name: Option<String>,
    },
    Test {
        path: Option<PathBuf>,
    },
    Export {
        run_id: String,
        #[arg(long)]
        out: Option<PathBuf>,
    },
    Import {
        file: PathBuf,
    },
    Doctor,
}

#[derive(Debug, Serialize, Deserialize)]
struct Config {
    version: u32,
    project: ProjectConfig,
    capture: CaptureConfig,
    redaction: RedactionConfig,
    cloud: CloudConfig,
}
#[derive(Debug, Serialize, Deserialize)]
struct ProjectConfig {
    name: String,
    workspace: String,
}
#[derive(Debug, Serialize, Deserialize)]
struct CaptureConfig {
    process: bool,
    filesystem: bool,
}
#[derive(Debug, Serialize, Deserialize)]
struct RedactionConfig {
    enabled: bool,
    env_values: bool,
}
#[derive(Debug, Serialize, Deserialize)]
struct CloudConfig {
    enabled: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct TestCase {
    version: u32,
    name: String,
    source_run: String,
    assertions: Vec<Assertion>,
}
#[derive(Debug, Serialize, Deserialize)]
struct Assertion {
    #[serde(rename = "type")]
    kind: String,
    #[serde(rename = "where")]
    where_: Option<serde_json::Value>,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error:#}");
        std::process::exit(3);
    }
}

fn run() -> Result<()> {
    match Cli::parse().command {
        Commands::Init => init(),
        Commands::Record { command } => record(command),
        Commands::List => list(),
        Commands::Inspect { run_id, events } => inspect(&run_id, events),
        Commands::Promote { run_id, name } => promote(&run_id, name),
        Commands::Test { path } => test_cases(path.as_deref()),
        Commands::Export { run_id, out } => export(&run_id, out.as_deref()),
        Commands::Import { file } => import(&file),
        Commands::Doctor => doctor(),
    }
}

fn init() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let name = cwd
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("agentflight-project")
        .to_string();
    let dir = cwd.join(".agentflight");
    fs::create_dir_all(&dir)?;
    let config = Config {
        version: 1,
        project: ProjectConfig {
            name,
            workspace: ".".into(),
        },
        capture: CaptureConfig {
            process: true,
            filesystem: true,
        },
        redaction: RedactionConfig {
            enabled: true,
            env_values: true,
        },
        cloud: CloudConfig { enabled: false },
    };
    let path = dir.join("config.yaml");
    if path.exists() {
        bail!("{} already exists", path.display());
    }
    fs::write(&path, serde_yaml::to_string(&config)?)?;
    fs::create_dir_all(cwd.join("tests/agentflight"))?;
    println!("Initialized AgentFlight in {}", path.display());
    Ok(())
}

fn load_config(cwd: &Path) -> Result<Config> {
    let path = cwd.join(".agentflight/config.yaml");
    if !path.exists() {
        bail!("project is not initialized; run `agentflight init`");
    }
    Ok(serde_yaml::from_str(&fs::read_to_string(path)?)?)
}

fn record(command: Vec<String>) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let config = load_config(&cwd)?;
    let redactor = Redactor::standard();
    let (redacted_command, initial_redactions) = redact_arguments(&redactor, &command);
    let mut manifest = RunManifest::new(config.project.name, redacted_command, &cwd);
    manifest.redaction_count = initial_redactions;
    let run_dir = data_home()?
        .join("projects")
        .join(project_id(&cwd))
        .join("runs")
        .join(&manifest.run_id);
    fs::create_dir_all(&run_dir)?;
    write_json(&run_dir.join("manifest.json"), &manifest)?;
    project_store()?.upsert_run(&manifest)?;
    let before = snapshot(&cwd)?;
    let events_path = run_dir.join("events.ndjson");
    let mut journal = RunJournal::open(&run_dir.join("journal.log"))?;
    let artifacts = ArtifactStore::new(project_root()?.join("blobs"));
    let mut sequence = 1;
    let (payload, count) = redact_json(&redactor, json!({"command": command, "cwd": cwd}));
    manifest.redaction_count += count;
    persist_event(
        &mut journal,
        &events_path,
        &Event::new(&manifest.run_id, sequence, "process.start", payload),
    )?;
    sequence += 1;
    let capture = run_pty(&command, &cwd)?;
    if !capture.output.is_empty() {
        let terminal_output = String::from_utf8_lossy(&capture.output);
        let (redacted, count) = redactor.redact(&terminal_output);
        manifest.redaction_count += count;
        let artifact = artifacts.put(redacted.as_bytes())?;
        let digest = artifact.strip_prefix("blake3:").unwrap();
        artifacts.materialize(&artifact, &run_dir.join("artifacts").join(digest))?;
        let mut event = Event::new(
            &manifest.run_id,
            sequence,
            "process.output",
            json!({
                "stream": "pty",
                "encoding": "utf-8",
                "byte_count": redacted.len(),
                "preview": preview(&redacted, 4096)
            }),
        );
        event.artifact_refs.push(artifact);
        persist_event(&mut journal, &events_path, &event)?;
        sequence += 1;
    }
    let after = snapshot(&cwd)?;
    for event in file_change_events(&manifest.run_id, sequence, &before, &after) {
        persist_event(&mut journal, &events_path, &event)?;
        sequence = event.sequence + 1;
    }
    persist_event(
        &mut journal,
        &events_path,
        &Event::new(
            &manifest.run_id,
            sequence,
            "process.exit",
            json!({"exit_code": capture.exit_code}),
        ),
    )?;
    journal.checkpoint(sequence)?;
    manifest.event_count = sequence;
    manifest.exit_code = Some(capture.exit_code as i32);
    manifest.status = if capture.success {
        RunStatus::Succeeded
    } else {
        RunStatus::Failed
    };
    manifest.ended_at = Some(chrono::Utc::now());
    write_json(&run_dir.join("manifest.json"), &manifest)?;
    project_store()?.upsert_run(&manifest)?;
    println!(
        "\nRecorded {} ({:?}, {} events)",
        manifest.run_id, manifest.status, manifest.event_count
    );
    println!("Inspect: agentflight inspect {} --events", manifest.run_id);
    if !capture.success {
        std::process::exit(capture.exit_code.min(255) as i32);
    }
    Ok(())
}

fn persist_event(journal: &mut RunJournal, events_path: &Path, event: &Event) -> Result<()> {
    journal.append(event)?;
    append_event(events_path, event)
}

fn preview(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

fn redact_arguments(redactor: &Redactor, arguments: &[String]) -> (Vec<String>, u64) {
    let mut total = 0;
    let arguments = arguments
        .iter()
        .map(|argument| {
            let (redacted, count) = redactor.redact(argument);
            total += count;
            redacted
        })
        .collect();
    (arguments, total)
}

fn list() -> Result<()> {
    let manifests = project_store()?.list_runs(100)?;
    if manifests.is_empty() {
        println!("No runs recorded.");
        return Ok(());
    }
    for m in manifests {
        println!(
            "{}  {:?}  exit={:?}  events={}  {}",
            m.run_id,
            m.status,
            m.exit_code,
            m.event_count,
            m.command.join(" ")
        );
    }
    Ok(())
}

fn inspect(run_id: &str, show_events: bool) -> Result<()> {
    let dir = resolve_run(run_id)?;
    let manifest: RunManifest = serde_json::from_slice(&fs::read(dir.join("manifest.json"))?)?;
    println!(
        "Run: {}\nStatus: {:?}\nCommand: {}\nStarted: {}\nEvents: {}\nRedactions: {}",
        manifest.run_id,
        manifest.status,
        manifest.command.join(" "),
        manifest.started_at,
        manifest.event_count,
        manifest.redaction_count
    );
    if show_events {
        let events_path = dir.join("events.ndjson");
        let events = match read_events(&events_path) {
            Ok(events) => events,
            Err(error) if dir.join("journal.log").exists() => {
                let recovered = RunJournal::recover(&dir.join("journal.log"), &events_path)?;
                eprintln!("Recovered {recovered} events from journal after: {error}");
                read_events(&events_path)?
            }
            Err(error) => return Err(error),
        };
        for event in events {
            let artifacts = if event.artifact_refs.is_empty() {
                String::new()
            } else {
                format!(" artifacts={}", event.artifact_refs.join(","))
            };
            println!(
                "{:>5}  {:<20} {}{}",
                event.sequence,
                event.event_type,
                serde_json::to_string(&event.payload)?,
                artifacts
            );
        }
    }
    Ok(())
}

fn promote(run_id: &str, name: Option<String>) -> Result<()> {
    let dir = resolve_run(run_id)?;
    let manifest: RunManifest = serde_json::from_slice(&fs::read(dir.join("manifest.json"))?)?;
    let name = name.unwrap_or_else(|| format!("regression-{}", &manifest.run_id[4..12]));
    let case = TestCase {
        version: 1,
        name: name.clone(),
        source_run: manifest.run_id,
        assertions: vec![Assertion {
            kind: "event.absent".into(),
            where_: Some(json!({"event_type": "file.change", "payload.change": "deleted"})),
        }],
    };
    let output = std::env::current_dir()?
        .join("tests/agentflight")
        .join(format!("{name}.yaml"));
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&output, serde_yaml::to_string(&case)?)?;
    println!("Created {}", output.display());
    Ok(())
}

fn test_cases(path: Option<&Path>) -> Result<()> {
    let root = path
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("tests/agentflight"));
    let files = if root.is_file() {
        vec![root]
    } else {
        fs::read_dir(&root)
            .with_context(|| format!("read {}", root.display()))?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| matches!(p.extension().and_then(|x| x.to_str()), Some("yaml" | "yml")))
            .collect()
    };
    let mut failures = 0;
    for file in files {
        let case: TestCase = serde_yaml::from_str(&fs::read_to_string(&file)?)?;
        let events = read_events(&resolve_run(&case.source_run)?.join("events.ndjson"))?;
        let mut passed = true;
        for assertion in &case.assertions {
            if assertion.kind == "event.absent" {
                if events
                    .iter()
                    .any(|e| matches_where(e, assertion.where_.as_ref()))
                {
                    passed = false;
                }
            } else {
                bail!("unsupported assertion type: {}", assertion.kind);
            }
        }
        println!("{} {}", if passed { "PASS" } else { "FAIL" }, case.name);
        if !passed {
            failures += 1;
        }
    }
    if failures > 0 {
        std::process::exit(1);
    }
    Ok(())
}

fn matches_where(event: &Event, filter: Option<&serde_json::Value>) -> bool {
    let Some(filter) = filter.and_then(|v| v.as_object()) else {
        return true;
    };
    filter.iter().all(|(key, expected)| {
        if key == "event_type" {
            expected == &json!(event.event_type)
        } else if let Some(path) = key.strip_prefix("payload.") {
            event
                .payload
                .pointer(&format!("/{}", path.replace('.', "/")))
                == Some(expected)
        } else {
            false
        }
    })
}

fn export(run_id: &str, out: Option<&Path>) -> Result<()> {
    let dir = resolve_run(run_id)?;
    let output = out
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("{run_id}.afrun")));
    export_run(&dir, &output)?;
    println!("Exported {}", output.display());
    Ok(())
}
fn import(file: &Path) -> Result<()> {
    let temp = data_home()?
        .join("imports")
        .join(format!("tmp-{}", std::process::id()));
    if temp.exists() {
        fs::remove_dir_all(&temp)?;
    }
    import_run(file, &temp)?;
    let manifest: RunManifest = serde_json::from_slice(&fs::read(temp.join("manifest.json"))?)?;
    let dest = project_runs()?.join(&manifest.run_id);
    if dest.exists() {
        bail!("run {} already exists", manifest.run_id);
    }
    fs::create_dir_all(dest.parent().unwrap())?;
    fs::rename(temp, &dest)?;
    project_store()?.upsert_run(&manifest)?;
    println!("Imported {}", manifest.run_id);
    Ok(())
}

fn doctor() -> Result<()> {
    let cwd = std::env::current_dir()?;
    println!(
        "AgentFlight doctor\n  config: {}\n  data: {}\n  cloud: disabled (default)\n  platform: {}-{}",
        if cwd.join(".agentflight/config.yaml").exists() {
            "ok"
        } else {
            "missing"
        },
        data_home()?.display(),
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    Ok(())
}
fn project_id(cwd: &Path) -> String {
    let hash = blake3::hash(cwd.to_string_lossy().as_bytes())
        .to_hex()
        .to_string();
    hash[..16].to_string()
}
fn project_runs() -> Result<PathBuf> {
    Ok(project_root()?.join("runs"))
}
fn project_root() -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    Ok(data_home()?.join("projects").join(project_id(&cwd)))
}
fn project_store() -> Result<MetadataStore> {
    MetadataStore::open(&project_root()?.join("metadata.db"))
}
fn resolve_run(run_id: &str) -> Result<PathBuf> {
    let runs = project_runs()?;
    if run_id == "latest" {
        return fs::read_dir(&runs)?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.join("manifest.json").exists())
            .max_by_key(|p| {
                fs::metadata(p.join("manifest.json"))
                    .and_then(|m| m.modified())
                    .ok()
            })
            .context("no runs recorded");
    }
    let dir = runs.join(run_id);
    if !dir.exists() {
        bail!("run not found: {run_id}");
    }
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn event_filter_matches_nested_payload() {
        let event = Event::new("run", 1, "file.change", json!({"change":"deleted"}));
        assert!(matches_where(
            &event,
            Some(&json!({"event_type":"file.change","payload.change":"deleted"}))
        ));
    }

    #[test]
    fn redacts_secrets_from_persisted_command_arguments() {
        let (arguments, count) = redact_arguments(
            &Redactor::standard(),
            &["--token=sk-abcdefghijklmnopqrstuv".into()],
        );
        assert_eq!(count, 1);
        assert!(!arguments[0].contains("abcdefghijklmnopqrstuv"));
    }
}
