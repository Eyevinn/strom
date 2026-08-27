#!/bin/bash
# Install Git hooks for Strom project
# Run this script after cloning the repository to set up pre-commit hooks

set -e

echo "Installing Git hooks for Strom..."

# Resolve the hooks directory. --git-common-dir rather than a literal .git:
# inside a worktree .git is a file, not a directory, and the hooks live in the
# main checkout's git dir shared by every worktree.
GIT_COMMON_DIR=$(git rev-parse --git-common-dir 2>/dev/null) || {
    echo "❌ Error: not inside a git repository"
    exit 1
}
HOOKS_DIR="$GIT_COMMON_DIR/hooks"

# core.hooksPath overrides the hooks directory, so installing there would be
# silently ignored. Fail loudly instead of pretending the hook is active.
CONFIGURED_PATH=$(git config --get core.hooksPath || true)
if [ -n "$CONFIGURED_PATH" ]; then
    echo "❌ Error: core.hooksPath is set to '$CONFIGURED_PATH'"
    echo "   A hook in $HOOKS_DIR would be ignored. Install into that path"
    echo "   instead, or unset it: git config --unset core.hooksPath"
    exit 1
fi

# Locate the hook source relative to this script, so the installer works from
# any directory.
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)

echo "→ Installing pre-commit hook..."
mkdir -p "$HOOKS_DIR"
cp "$SCRIPT_DIR/pre-commit" "$HOOKS_DIR/pre-commit"
chmod +x "$HOOKS_DIR/pre-commit"

echo ""
echo "✅ Git hooks installed successfully!"
echo ""
echo "Installed to: $HOOKS_DIR/pre-commit"
echo ""
echo "The following checks will run before each commit:"
echo "  • cargo fmt --all (code formatting)"
echo "  • cargo clippy --workspace --all-targets --features efp,nvidia"
echo "  • cargo clippy for wasm32, when frontend/ is staged"
echo "  • claude (sensitive content check, if installed)"
echo ""
echo "To skip these checks temporarily, use: git commit --no-verify"
echo ""
