# Grove — development and setup recipes.
#
# `just` with no arguments lists them.

default:
    @just --list

# ------------------------------------------------------------------ the gate

# Everything CLAUDE.md requires before a commit.
gate: build build-minimal test clippy fmt-check

build:
    cargo build --workspace

# The picker is feature-gated; building without it is part of the gate.
build-minimal:
    cargo build -p grove --no-default-features

test:
    cargo test --workspace

clippy:
    cargo clippy --workspace --all-targets -- -D warnings

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all --check

# Run the GUI.
run:
    cargo run -p grove

# Rebuild and rerun the GUI on every change. Needs `cargo install cargo-watch`.
watch:
    cargo watch -x 'run -p grove'

# ------------------------------------------------------- Claude Code hooks
#
# The hooks themselves live in the binary now (`grove hooks`), so they work
# from a `cargo install` with no checkout to run `just` in, and the merge into
# settings.json is the same tested code the Settings pane uses. These recipes
# are conveniences that run the built binary.

# Show whether Grove's hooks are installed in Claude Code's settings.
hooks:
    cargo run -q -p grove -- hooks status

# Merge them in. Backs settings.json up first and leaves your own hooks alone.
install-claude-hook:
    cargo run -q -p grove -- hooks install

# Take them back out.
uninstall-claude-hook:
    cargo run -q -p grove -- hooks uninstall

# Show what would be added, changing nothing.
print-claude-hook:
    cargo run -q -p grove -- hooks print

# ------------------------------------------------------------------ install

# Install the grove binary into ~/.cargo/bin.
install:
    cargo install --path crates/grove --locked
