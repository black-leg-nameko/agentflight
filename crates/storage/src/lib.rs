use agentflight_core::RunManifest;
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;

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
}
