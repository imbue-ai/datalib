#!/bin/bash
# Set up git hooks for the repository.
set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
GIT_DIR="$(git rev-parse --git-dir)"

mkdir -p "$GIT_DIR/hooks"
ln -sf "$SCRIPT_DIR/pre-commit" "$GIT_DIR/hooks/pre-commit"
chmod +x "$SCRIPT_DIR/pre-commit"

echo "Pre-commit hook installed at $GIT_DIR/hooks/pre-commit"
echo "It will run: ruff, pyright, vue-tsc, cargo fmt, cargo clippy"
