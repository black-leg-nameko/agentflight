# AgentFlight

AgentFlight is a local-first flight recorder for AI-agent runs. It captures a command's lifecycle, stdout/stderr, workspace file changes, and turns recorded failures into regression assertions. Cloud communication is disabled by design in this initial implementation.

This repository currently implements the Phase 0 vertical slice from the product specification:

- public Event and Run Manifest JSON Schemas;
- a Rust CLI with `init`, `record`, `list`, `inspect`, `promote`, `test`, `export`, `import`, and `doctor`;
- pre-persistence redaction for common tokens and secret environment values;
- checksum-verified, path-traversal-safe `.afrun` bundles;
- a deliberately faulty sample agent and a short local demo.

## Build

```sh
cargo build --release
export PATH="$PWD/target/release:$PATH"
agentflight doctor
```

Rust 1.85 or newer is recommended. AgentFlight stores runs under `~/.agentflight` by default. Set `AGENTFLIGHT_HOME` to choose another location.

## 30-second demo

Run this from the repository root:

```sh
agentflight init
chmod +x examples/demo-agent.sh
agentflight record -- ./examples/demo-agent.sh broken
agentflight inspect latest --events
agentflight promote latest --name no-delete-config
agentflight test tests/agentflight/no-delete-config.yaml
```

The last command exits with code `1`, proving that the recorded run deleted a configuration file. The generated YAML is intentionally simple and can be committed as a regression artifact.

Export and validate the run as a portable bundle:

```sh
mkdir -p out
agentflight export latest --out out/demo.afrun
```

## Data and safety

Recording excludes `.git`, `.agentflight`, `target`, `node_modules`, and existing `.afrun` files. This phase records file metadata and hashes, not file contents. Bundle import validates checksums, rejects absolute and parent-directory paths, and never executes bundle content.

AgentFlight cannot guarantee complete secret detection or full sandboxing. Review bundles before sharing them. Live replay and cloud upload are not implemented or enabled.

## Project status

Version 0.1 is an early implementation. PTY capture, SQLite storage, MCP proxying, the Timeline UI, mock replay, richer assertions, and semantic diff remain roadmap work.

Licensed under Apache-2.0.
