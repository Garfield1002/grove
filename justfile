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

# The icon is the scalable SVG only. Modern desktops render it at whatever
# size they need, and the fine line art turns to mush below about 48px
# anyway, so shipping small PNGs would buy nothing.

# Install the desktop entry and icon, so Grove has a launcher entry and a real window icon.
install-desktop:
    install -Dm644 packaging/grove.desktop \
        "${XDG_DATA_HOME:-$HOME/.local/share}/applications/grove.desktop"
    install -Dm644 packaging/grove.svg \
        "${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor/scalable/apps/grove.svg"
    -update-desktop-database "${XDG_DATA_HOME:-$HOME/.local/share}/applications" 2>/dev/null
    -gtk-update-icon-cache -f -t "${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor" 2>/dev/null
    @echo "Installed. Grove's window pairs with this entry through its app_id."

# Remove both again.
uninstall-desktop:
    rm -f "${XDG_DATA_HOME:-$HOME/.local/share}/applications/grove.desktop"
    rm -f "${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor/scalable/apps/grove.svg"
    -update-desktop-database "${XDG_DATA_HOME:-$HOME/.local/share}/applications" 2>/dev/null
