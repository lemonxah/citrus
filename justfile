set shell := ["bash", "-euo", "pipefail", "-c"]

# Show available commands.
default:
    @just --list

# Show the shared workspace version.
current:
    @python3 tools/version.py

# Bump the shared workspace version and refresh Cargo.lock (no dependency upgrades).
bump new_version:
    python3 tools/version.py {{quote(new_version)}}

# Run all workspace tests.
test:
    cargo test --workspace --locked

# Check every workspace target without producing executables.
check:
    cargo check --workspace --all-targets --locked

# Format Rust sources.
fmt:
    cargo fmt --all

# Build the workspace, including the editor, hub, relay and example plugin.
build:
    cargo build --workspace --release --locked

# Run the editor (build both executables so switching to the hub works).
[positional-arguments]
run *args:
    cargo build -p citrus -p citrus-hub --locked
    cargo run --bin citrus --locked -- "$@"

# Open the Project Hub with its matching editor built.
hub:
    cargo build -p citrus -p citrus-hub --locked
    cargo run --bin citrus-hub --locked

# Build and install on Linux; override with: just install "$HOME/.local" ""
install prefix="/usr" sudo="sudo": build
    bash tools/install.sh {{quote(prefix)}} {{quote(sudo)}}

# Bump, commit, tag, push and create a GitHub release (requires a clean checkout).
release new_version msg="":
    #!/usr/bin/env bash
    set -euo pipefail
    version={{quote(new_version)}}
    message={{quote(msg)}}
    command -v gh >/dev/null
    gh auth status
    if [[ -n "$(git status --porcelain)" ]]; then
        echo "Commit or stash your changes before releasing." >&2
        exit 1
    fi
    branch=$(git symbolic-ref --short HEAD)
    git rev-parse --verify '@{upstream}' >/dev/null
    if git show-ref --verify --quiet "refs/tags/v$version"; then
        echo "Tag v$version already exists." >&2
        exit 1
    fi
    just bump "$version"
    title="v$version"
    if [[ -n "$message" ]]; then title+=": $message"; fi
    git add -- Cargo.toml Cargo.lock
    git commit -m "$title" -- Cargo.toml Cargo.lock
    git tag -a "v$version" -m "$title"
    git push --atomic origin "$branch" "refs/tags/v$version"
    gh release create "v$version" --verify-tag --title "$title" --generate-notes
