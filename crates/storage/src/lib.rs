use agentflight_core::{Event, RunManifest};
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

const MIGRATION_V1: &str = include_str!("../migrations/001_initial.sql");

pub struct MetadataStore {
    connection: Connection,
}

impl MetadataStore {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(path)
            .with_context(|| format!("open metadata database {}", path.display()))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "foreign_keys", true)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        let store = Self { connection };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        let version: i64 = self
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version < 1 {
            self.connection.execute_batch(MIGRATION_V1)?;
        }
        Ok(())
    }

    pub fn upsert_run(&self, manifest: &RunManifest) -> Result<()> {
        let json = serde_json::to_string(manifest)?;
        self.connection.execute(
            "INSERT INTO runs (run_id, started_at, status, project, command, event_count, manifest_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(run_id) DO UPDATE SET
               status = excluded.status,
               event_count = excluded.event_count,
               manifest_json = excluded.manifest_json",
            params![
                manifest.run_id,
                manifest.started_at.to_rfc3339(),
                status_name(manifest),
                manifest.project,
                manifest.command.join(" "),
                manifest.event_count,
                json
            ],
        )?;
        Ok(())
    }

    pub fn list_runs(&self, limit: u32) -> Result<Vec<RunManifest>> {
        let mut statement = self.connection.prepare(
            "SELECT manifest_json FROM runs ORDER BY started_at DESC, run_id DESC LIMIT ?1",
        )?;
        let rows = statement.query_map([limit], |row| row.get::<_, String>(0))?;
        rows.map(|row| {
            let json = row?;
            serde_json::from_str(&json).map_err(Into::into)
        })
        .collect()
    }

    pub fn get_run(&self, run_id: &str) -> Result<Option<RunManifest>> {
        let json = self
            .connection
            .query_row(
                "SELECT manifest_json FROM runs WHERE run_id = ?1",
                [run_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        json.map(|value| serde_json::from_str(&value).map_err(Into::into))
            .transpose()
    }
}

fn status_name(manifest: &RunManifest) -> &'static str {
    use agentflight_core::RunStatus;
    match manifest.status {
        RunStatus::Running => "running",
        RunStatus::Succeeded => "succeeded",
        RunStatus::Failed => "failed",
        RunStatus::Interrupted => "interrupted",
    }
}

pub struct ArtifactStore {
    root: PathBuf,
}

impl ArtifactStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn put(&self, bytes: &[u8]) -> Result<String> {
        let digest = blake3::hash(bytes).to_hex().to_string();
        let destination = self.root.join(&digest[..2]).join(&digest[2..]);
        if !destination.exists() {
            fs::create_dir_all(destination.parent().unwrap())?;
            let temporary = destination.with_extension(format!("tmp-{}", std::process::id()));
            fs::write(&temporary, bytes)?;
            match fs::rename(&temporary, &destination) {
                Ok(()) => {}
                Err(_) if destination.exists() => {
                    let _ = fs::remove_file(temporary);
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(format!("blake3:{digest}"))
    }

    pub fn read(&self, reference: &str) -> Result<Vec<u8>> {
        let digest = reference
            .strip_prefix("blake3:")
            .context("artifact reference must start with blake3:")?;
        if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            anyhow::bail!("invalid artifact digest");
        }
        let bytes = fs::read(self.root.join(&digest[..2]).join(&digest[2..]))?;
        if blake3::hash(&bytes).to_hex().as_str() != digest {
            anyhow::bail!("artifact checksum mismatch");
        }
        Ok(bytes)
    }

    pub fn materialize(&self, reference: &str, destination: &Path) -> Result<()> {
        let bytes = self.read(reference)?;
        fs::create_dir_all(
            destination
                .parent()
                .context("artifact destination has no parent")?,
        )?;
        fs::write(destination, bytes)?;
        Ok(())
    }
}

pub struct RunJournal {
    file: fs::File,
}

impl RunJournal {
    pub fn open(path: &Path) -> Result<Self> {
        let file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(path)?;
        Ok(Self { file })
    }

    pub fn append(&mut self, event: &Event) -> Result<()> {
        serde_json::to_writer(&mut self.file, event)?;
        writeln!(self.file)?;
        self.file.sync_data()?;
        Ok(())
    }

    pub fn checkpoint(&mut self, sequence: u64) -> Result<()> {
        writeln!(self.file, "{{\"checkpoint\":{sequence}}}")?;
        self.file.sync_all()?;
        Ok(())
    }

    pub fn recover(journal_path: &Path, events_path: &Path) -> Result<usize> {
        let journal = fs::read_to_string(journal_path)?;
        let events = journal
            .lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| serde_json::from_str::<Event>(line).ok())
            .collect::<Vec<_>>();
        if events.is_empty() {
            anyhow::bail!("journal contains no recoverable events");
        }
        let temporary = events_path.with_extension(format!("recover-{}", std::process::id()));
        let mut file = fs::File::create(&temporary)?;
        for event in &events {
            serde_json::to_writer(&mut file, event)?;
            writeln!(file)?;
        }
        file.sync_all()?;
        fs::rename(temporary, events_path)?;
        Ok(events.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentflight_core::{RunManifest, RunStatus};

    #[test]
    fn persists_and_updates_a_run() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = MetadataStore::open(&temp.path().join("metadata.db"))?;
        let mut manifest = RunManifest::new(
            "test".into(),
            vec!["echo".into(), "hello".into()],
            temp.path(),
        );
        store.upsert_run(&manifest)?;
        manifest.status = RunStatus::Succeeded;
        manifest.event_count = 2;
        store.upsert_run(&manifest)?;

        let loaded = store.get_run(&manifest.run_id)?.unwrap();
        assert_eq!(loaded.status, RunStatus::Succeeded);
        assert_eq!(loaded.event_count, 2);
        assert_eq!(store.list_runs(10)?.len(), 1);
        Ok(())
    }

    #[test]
    fn enables_wal_mode() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("metadata.db");
        let _store = MetadataStore::open(&path)?;
        let connection = Connection::open(path)?;
        let mode: String = connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
        assert_eq!(mode.to_ascii_lowercase(), "wal");
        Ok(())
    }

    #[test]
    fn deduplicates_and_verifies_artifacts() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = ArtifactStore::new(temp.path());
        let first = store.put(b"terminal output")?;
        let second = store.put(b"terminal output")?;
        assert_eq!(first, second);
        assert_eq!(store.read(&first)?, b"terminal output");
        Ok(())
    }

    #[test]
    fn recovers_events_from_the_journal() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let journal_path = temp.path().join("journal.log");
        let events_path = temp.path().join("events.ndjson");
        let mut journal = RunJournal::open(&journal_path)?;
        journal.append(&Event::new(
            "run_test",
            1,
            "process.start",
            serde_json::json!({}),
        ))?;
        journal.checkpoint(1)?;
        fs::write(&events_path, b"truncated")?;

        assert_eq!(RunJournal::recover(&journal_path, &events_path)?, 1);
        let recovered = agentflight_core::read_events(&events_path)?;
        assert_eq!(recovered[0].sequence, 1);
        Ok(())
    }
}
