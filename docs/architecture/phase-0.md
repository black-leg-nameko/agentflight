# Phase 0 architecture

The first implementation keeps public formats independent from capture and storage details.

1. `agentflight-core` owns Run/Event types, sequence helpers, redaction, and workspace snapshots.
2. `agentflight-bundle` owns bundle checksums and safe extraction.
3. `agentflight-cli` composes these pieces into the initial user journey.
4. `schemas/` is the language-independent compatibility boundary.

Each run is currently stored at:

```text
~/.agentflight/projects/<project-hash>/runs/<run-id>/
  manifest.json
  events.ndjson
~/.agentflight/projects/<project-hash>/metadata.db
```

SQLite WAL indexes Run metadata and is managed by versioned SQL migrations. The per-Run files remain the journal and public bundle source, keeping exported formats independent from SQLite internals.

## Event ordering

Sequence numbers, rather than wall-clock timestamps, are authoritative within a run. The recorder writes append-only NDJSON and syncs each event before continuing. The final manifest is written after process output and file-change capture.

## Known boundaries

The process adapter currently uses stdout/stderr pipes rather than a PTY and emits one aggregate event per stream. Workspace capture reports added, modified, and deleted paths with BLAKE3 hashes, but does not yet store content artifacts or intermediate watcher states.
