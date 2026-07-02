#!/bin/bash
# Set up git hooks for the repository.
set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# Install into the *common* hooks dir, not `git rev-parse --git-dir`.
# In a linked worktree `--git-dir` points at .git/worktrees/<name>, but
# git only executes hooks from the shared common dir (.git/hooks) unless
# core.hooksPath is set — so installing to the per-worktree dir silently
# does nothing. --git-common-dir resolves to .git/hooks in both a normal
# clone and a worktree.
HOOKS_DIR="$(git rev-parse --git-common-dir)/hooks"

mkdir -p "$HOOKS_DIR"
ln -sf "$SCRIPT_DIR/pre-commit" "$HOOKS_DIR/pre-commit"
chmod +x "$SCRIPT_DIR/pre-commit"
ln -sf "$SCRIPT_DIR/pre-push" "$HOOKS_DIR/pre-push"
chmod +x "$SCRIPT_DIR/pre-push"

echo "Pre-commit hook installed at $HOOKS_DIR/pre-commit"
echo "It will run: ruff check + format (fast, auto-fixing)"
echo "Pre-push hook installed at $HOOKS_DIR/pre-push"
echo "It will run: ruff, pyright, vue-tsc, cargo fmt, cargo clippy, rust unit-test coverage"
