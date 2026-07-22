BEGIN;

CREATE TABLE IF NOT EXISTS runs (
  run_id TEXT PRIMARY KEY,
  started_at TEXT NOT NULL,
  status TEXT NOT NULL CHECK (status IN ('running', 'succeeded', 'failed', 'interrupted')),
  project TEXT NOT NULL,
  command TEXT NOT NULL,
  event_count INTEGER NOT NULL DEFAULT 0,
  manifest_json TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS runs_started_at_idx ON runs(started_at DESC);

PRAGMA user_version = 1;
COMMIT;
