#!/bin/bash
# Install Git hooks for Strom project
# Run this script after cloning the repository to set up pre-commit hooks

set -e

echo "Installing Git hooks for Strom..."

# Check if we're in a git repository
if [ ! -d ".git" ]; then
    echo "❌ Error: This script must be run from the root of the git repository"
    exit 1
fi

# Copy pre-commit hook
echo "→ Installing pre-commit hook..."
cp scripts/pre-commit .git/hooks/pre-commit
chmod +x .git/hooks/pre-commit

echo ""
echo "✅ Git hooks installed successfully!"
echo ""
echo "The following checks will run before each commit:"
echo "  • cargo fmt (code formatting)"
echo "  • cargo clippy (linting)"
echo "  • claude (sensitive content check, if installed)"
echo ""
echo "To skip these checks temporarily, use: git commit --no-verify"
echo ""
