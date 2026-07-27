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

# Where Claude Code keeps its user-level settings.
claude_settings := env('CLAUDE_CONFIG_DIR', env('HOME') / '.claude') / 'settings.json'

# Notification fires when Claude wants permission or input — exactly the
# "needs attention" signal Grove cannot infer for itself. Stop fires when a
# turn ends, and UserPromptSubmit when one begins.
#
# GROVE_SESSION does not appear anywhere below on purpose: every Grove tmux
# session exports it, so a hook running inside one already has it, and a
# Claude Code session started outside Grove correctly reports nothing.
#
# Show the Claude Code hook configuration, changing nothing.
print-claude-hook:
    @printf '%s\n' \
      '{' \
      '  "hooks": {' \
      '    "Notification": [' \
      '      { "matcher": "", "hooks": [ { "type": "command", "command": "grove notify --state attention" } ] }' \
      '    ],' \
      '    "Stop": [' \
      '      { "matcher": "", "hooks": [ { "type": "command", "command": "grove notify --state idle" } ] }' \
      '    ],' \
      '    "UserPromptSubmit": [' \
      '      { "matcher": "", "hooks": [ { "type": "command", "command": "grove notify --state working" } ] }' \
      '    ]' \
      '  }' \
      '}'

# Backs the file up first and merges rather than overwriting, so hooks you
# already have survive. Review with `just print-claude-hook` before running.
#
# Merge the Grove hooks into Claude Code's settings.json.
install-claude-hook:
    #!/usr/bin/env bash
    set -euo pipefail
    settings="{{ claude_settings }}"
    command -v jq >/dev/null || { echo "just: jq is required" >&2; exit 1; }
    command -v grove >/dev/null || \
      echo "warning: no 'grove' on PATH — the hooks will not fire until there is one (try 'just install')" >&2
    mkdir -p "$(dirname "$settings")"
    [ -f "$settings" ] || echo '{}' > "$settings"
    backup="$settings.$(date +%Y%m%d%H%M%S).bak"
    cp "$settings" "$backup"
    echo "backed up $settings to $backup"
    # Append to each event's existing array rather than replacing it, and drop
    # any previous Grove entry first so re-running stays idempotent.
    just print-claude-hook | jq -s '
      .[0] as $grove | .[1] as $current |
      $current * {hooks: (
        ($current.hooks // {}) as $existing |
        reduce ($grove.hooks | keys[]) as $event ($existing;
          .[$event] = (
            (($existing[$event] // []) | map(select(
              (.hooks // []) | all(.command // "" | startswith("grove notify") | not)
            )))
            + $grove.hooks[$event]
          )
        )
      )}' - "$settings" > "$settings.new"
    mv "$settings.new" "$settings"
    echo "installed Grove hooks into $settings"
    echo "restart Claude Code for them to take effect"

# Remove the Grove hooks again.
uninstall-claude-hook:
    #!/usr/bin/env bash
    set -euo pipefail
    settings="{{ claude_settings }}"
    [ -f "$settings" ] || { echo "no $settings"; exit 0; }
    command -v jq >/dev/null || { echo "just: jq is required" >&2; exit 1; }
    cp "$settings" "$settings.bak"
    jq '
      if .hooks then
        .hooks |= with_entries(
          .value |= map(select(
            (.hooks // []) | all(.command // "" | startswith("grove notify") | not)
          ))
        ) | .hooks |= with_entries(select(.value | length > 0))
      else . end' "$settings" > "$settings.new"
    mv "$settings.new" "$settings"
    echo "removed Grove hooks from $settings"

# ------------------------------------------------------------------ install

# Install the grove binary into ~/.cargo/bin.
install:
    cargo install --path crates/grove --locked
