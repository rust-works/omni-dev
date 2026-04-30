#!/bin/bash

# Build script that runs cargo build, fmt check, clippy, and tests.
# Exits on first failure. Mirrors what CI checks so snapshot drift
# (e.g. tests/snapshots/integration_test__help_all_output.snap) is
# caught locally rather than only in CI. See
# .claude/skills/update-snapshots/SKILL.md for handling drift.

set -e

echo "🔨 Building project..."
cargo build

echo "✅ Build successful!"

echo "🎨 Checking code formatting..."
cargo fmt --check

echo "✅ Code formatting check passed!"

echo "🔍 Running clippy checks..."
cargo clippy --all-targets --all-features -- -D warnings

echo "✅ Clippy checks passed!"

echo "🧪 Running tests..."
cargo test

echo "✅ All checks passed! 🎉"