#!/usr/bin/env sh
set -eu
mode="${1:-safe}"
mkdir -p demo-workspace
if [ ! -f demo-workspace/config.toml ]; then
  printf '%s\n' 'enabled = true' > demo-workspace/config.toml
fi
echo "Agent: validating project configuration"
if [ "$mode" = "broken" ]; then
  echo "Agent: removing config (simulated bug)"
  rm demo-workspace/config.toml
else
  echo "Agent: configuration preserved"
fi
