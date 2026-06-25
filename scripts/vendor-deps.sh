#!/usr/bin/env bash
#
# vendor-deps.sh — produce an offline-buildable dependency set for Imperium.
#
# WHY: sandboxed agent/CI sessions cannot reach static.crates.io (crate
# tarballs) and so cannot run `cargo test -p sim_core`. Running this on a
# machine WITH crates.io access vendors the locked dependency graph so those
# sessions can build fully offline. See docs/build-environment.md.
#
# USAGE (on a networked machine, from the repo root):
#   ./scripts/vendor-deps.sh
#
# It is idempotent and uses the committed Cargo.lock so the vendored set is the
# exact, deterministic graph the project pins.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

if ! command -v cargo >/dev/null 2>&1; then
  echo "error: cargo not found on PATH. Install Rust via https://rustup.rs first." >&2
  exit 1
fi

if [[ ! -f Cargo.lock ]]; then
  echo "error: Cargo.lock not found at repo root ($REPO_ROOT)." >&2
  exit 1
fi

VENDOR_DIR="vendor"
CONFIG_DIR=".cargo"
CONFIG_FILE="$CONFIG_DIR/config.toml"

echo ">> Vendoring locked dependencies into ./$VENDOR_DIR (this downloads ~650 crates)…"
mkdir -p "$CONFIG_DIR"

# `cargo vendor` prints the [source] replacement stanza on stdout; capture it so
# we can wire up (or just show) the config without clobbering an existing one.
STANZA="$(cargo vendor --locked "$VENDOR_DIR")"

echo
echo ">> Done. Vendored sources are in ./$VENDOR_DIR"
echo
echo "Add the following to $CONFIG_FILE to build offline:"
echo "------------------------------------------------------------"
echo "$STANZA"
echo "------------------------------------------------------------"
echo
echo "Notes:"
echo "  * ./$VENDOR_DIR is large; commit it only if you want offline clones."
echo "  * With the stanza in place, 'cargo test -p sim_core --offline' needs no network."
