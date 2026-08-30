#!/usr/bin/env bash
# Package the repository as a pi.dev plugin (pi package) tarball.
#
# pi.dev packages are directories with a package.json containing a `pi`
# manifest (extensions / skills / prompts). This script:
#
#   1. Builds the Rust binaries (sermcp + dutabo) in release mode.
#   2. Stages a clean plugin tree (no git history, no target/, no .dut-serial):
#        dist/pi-plugin/sermcp/
#          ├── package.json              (pi manifest, pi-package keyword)
#          ├── bin/sermcp     (Rust server — found by the extension)
#          ├── bin/dutabo                (CLI)
#          ├── pi-extensions/            (statusline + MCP client bridge)
#          ├── skills/                   (Agent Skill: embedded-debug)
#          └── .pi/prompts/              (prompt templates, e.g. /debug-boot)
#   3. Creates dist/sermcp-pi-plugin.tar.gz
#
# Install the plugin:
#
#   # local directory (dev):
#   pi install /path/to/sermcp
#   # or from the repo root: pi install .
#
#   # tarball (shares the same layout — point pi at the extracted dir):
#   tar xzf dist/sermcp-pi-plugin.tar.gz -C /tmp
#   pi install /tmp/sermcp
#
#   # git (once published):
#   pi install git:github.com/bitshelf/sermcp@v1
#
#   # npm (once published):
#   pi install npm:sermcp
#
# After install, restart pi (or /reload). The MCP client bridge extension
# spawns `bin/sermcp` (discovery order: $SERMCP_BIN →
# .mcp.json command → PATH → ~/.local/bin → project bin/ → package bin/)
# and registers all 28 serial_* tools. Verify:
#
#   pi -p "Call serial_mcp_diagnostics and report its output verbatim."
#
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"
DIST="$ROOT/dist"
PLUGIN_DIR="$DIST/pi-plugin/sermcp"

echo "==> Building Rust binaries (release)…"
(cd sermcp && cargo build --release --locked)

echo "==> Staging plugin tree…"
rm -rf "$PLUGIN_DIR"
mkdir -p "$PLUGIN_DIR/bin" "$PLUGIN_DIR/pi-extensions" "$PLUGIN_DIR/skills" "$PLUGIN_DIR/.pi/prompts"

cp "$ROOT/package.json" "$PLUGIN_DIR/package.json"
cp "sermcp/target/release/sermcp" "$PLUGIN_DIR/bin/"
cp "sermcp/target/release/dutabo" "$PLUGIN_DIR/bin/" 2>/dev/null || true

cp -R "$ROOT/pi-extensions/input-layout" "$PLUGIN_DIR/pi-extensions/"
cp -R "$ROOT/pi-extensions/sermcp" "$PLUGIN_DIR/pi-extensions/"
cp -R "$ROOT/skills"/* "$PLUGIN_DIR/skills/"
cp "$ROOT/.pi/prompts/"*.md "$PLUGIN_DIR/.pi/prompts/"

echo "==> Validating manifest…"
node -e '
  const p = require(process.argv[1]);
  if (!p.pi) { console.error("ERROR: no pi manifest"); process.exit(1); }
  if (!(p.keywords || []).includes("pi-package")) { console.error("ERROR: missing pi-package keyword"); process.exit(1); }
  console.log("manifest OK:", JSON.stringify(p.pi, null, 2));
' "$PLUGIN_DIR/package.json"

echo "==> Creating tarball…"
mkdir -p "$DIST"
tar -C "$PLUGIN_DIR/.." -czf "$DIST/sermcp-pi-plugin.tar.gz" sermcp

echo ""
echo "Plugin ready:"
echo "  dir:     $PLUGIN_DIR"
echo "  tarball: $DIST/sermcp-pi-plugin.tar.gz"
echo ""
echo "Install (pick one):"
echo "  pi install $PLUGIN_DIR"
echo "  tar xzf $DIST/sermcp-pi-plugin.tar.gz -C /tmp && pi install /tmp/sermcp"
echo "  pi install git:github.com/bitshelf/sermcp@v1   # once published"
echo ""
echo "Verify: pi -p \"Call serial_mcp_diagnostics and report its output verbatim.\""
