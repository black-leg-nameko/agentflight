#!/usr/bin/env sh
set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
test_root=$(mktemp -d)
trap 'rm -rf "$test_root"' EXIT HUP INT TERM

mkdir -p "$test_root/tests"
cp -R "$repo_root/examples" "$test_root/examples"
cp -R "$repo_root/tests/fixtures" "$test_root/tests/fixtures"
export AGENTFLIGHT_HOME="$test_root/data"
export PATH="$repo_root/target/debug:$PATH"

cd "$test_root"
agentflight init >/dev/null
agentflight record -- sh examples/mcp-demo-agent.sh >/dev/null
agentflight inspect latest --events > inspect.txt

grep -q 'mcp.tool.call' inspect.txt
grep -q 'mcp.tool.result' inspect.txt
grep -q 'Redactions: 1' inspect.txt
if grep -R -q 'abcdefghijklmnopqrstuv' data; then
  echo "unredacted MCP secret persisted" >&2
  exit 1
fi
