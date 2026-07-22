# Phase 1 capture foundation

AgentFlight runs commands through a platform-native pseudo-terminal using `portable-pty`. Terminal stdout and stderr are captured as one ordered byte stream, which matches what an interactive user observes. Input is forwarded when AgentFlight itself is attached to a terminal.

Terminal output follows the persistence pipeline:

1. capture the PTY byte stream;
2. decode it lossily as UTF-8 and apply secret redaction;
3. store the redacted bytes in the project-level content-addressed Blob Store;
4. materialize the same verified content in the Run's `artifacts/` directory for portable export;
5. write an Event containing a bounded preview and `blake3:<digest>` reference.

Original unredacted output is never written by AgentFlight. Binary-safe terminal capture and structured stdout/stderr separation are not promised because a PTY exposes a combined terminal stream.

## Journal ordering and recovery

Every Event is synced to `journal.log` before it is appended to `events.ndjson`. A successful Run writes a final sequence checkpoint. If inspection finds a missing or malformed Event stream, it rebuilds `events.ndjson` from valid journal Event records. SQLite is updated at Run start and completion but remains an index rather than the canonical Event source.

## Artifact layout

```text
projects/<project-id>/
  blobs/<first-two-hex>/<remaining-hex>
  runs/<run-id>/
    artifacts/<full-hex>
    events.ndjson
    journal.log
    manifest.json
```

Blob reads recompute BLAKE3 and reject checksum mismatches. Repeated content shares one project-level Blob.
