#!/bin/bash
# Setup a minimal workspace context for building the Lane crate standalone.
# The Lane repo is normally a submodule of the a3s workspace, so we need to
# recreate the workspace structure for standalone CI builds.
#
# This script restructures the CURRENT directory in-place:
#   Before: ./ = Lane repo root (Cargo.toml, src/, sdk/, ...)
#   After:  ./ = workspace root with crates/lane/

set -euo pipefail

# Save current directory contents to a temp location
TMPDIR="$(mktemp -d)"
cp -a . "$TMPDIR/lane"

# Clean current directory (except .git)
find . -maxdepth 1 ! -name '.' ! -name '.git' -exec rm -rf {} +

# Create workspace structure
mkdir -p crates

# Move Lane repo into crates/lane
cp -a "$TMPDIR/lane/." crates/lane/

# Create workspace root Cargo.toml
cat > Cargo.toml << 'EOF'
[workspace]
resolver = "2"
members = ["crates/lane"]

[workspace.package]
version = "0.1.0"
edition = "2021"
license = "MIT"
repository = "https://github.com/a3s-lab/a3s"
authors = ["A3S Lab"]

[workspace.dependencies]
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
thiserror = "1"
anyhow = "1"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
async-trait = "0.1"

[profile.release]
opt-level = 3
lto = "thin"
codegen-units = 1
strip = true
EOF

# Clean up
rm -rf "$TMPDIR"

echo "Workspace restructured. Lane crate at: crates/lane/"
