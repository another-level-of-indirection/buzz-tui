#!/usr/bin/env bash
# Dev launcher: builds the Rust daemon, then runs the Ink shell.
set -euo pipefail

cd "$(dirname "$0")"

echo "Building buzz-sessiond…"
cargo build -p buzz-sessiond --quiet

export BUZZ_SESSIOND_PATH="$(pwd)/target/debug/buzz-sessiond"
exec bun run packages/shell/index.ts
