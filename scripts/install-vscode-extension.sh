#!/bin/bash

# Build, package, and install the omni-dev VS Code companion extension
# from editors/vscode/ into a local editor.
#
# Steps: npm ci → npm run build → npm run package (vsce → .vsix) →
# <editor> --install-extension <vsix>.
#
# The target editor CLI defaults to `code`; override with the
# OMNI_DEV_VSCODE_BIN env var (e.g. `cursor`, `codium`, `windsurf`) or the
# --editor flag. Pass --skip-ci to reuse the existing node_modules.

set -euo pipefail

EXT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../editors/vscode" && pwd)"
EDITOR_BIN="${OMNI_DEV_VSCODE_BIN:-code}"
SKIP_CI=false

while [ $# -gt 0 ]; do
  case "$1" in
    --editor)
      EDITOR_BIN="$2"
      shift 2
      ;;
    --skip-ci)
      SKIP_CI=true
      shift
      ;;
    -h|--help)
      echo "Usage: $0 [--editor <cli>] [--skip-ci]"
      echo "  --editor <cli>  Editor CLI to install into (default: \$OMNI_DEV_VSCODE_BIN or 'code')"
      echo "  --skip-ci       Reuse existing node_modules (skip 'npm ci')"
      exit 0
      ;;
    *)
      echo "❌ Unknown argument: $1" >&2
      echo "Run '$0 --help' for usage." >&2
      exit 1
      ;;
  esac
done

if ! command -v "$EDITOR_BIN" >/dev/null 2>&1; then
  echo "❌ Editor CLI '$EDITOR_BIN' not found on PATH." >&2
  echo "   Install its shell command or set OMNI_DEV_VSCODE_BIN / pass --editor." >&2
  exit 1
fi

cd "$EXT_DIR"

if [ "$SKIP_CI" = false ]; then
  echo "📦 Installing dependencies (npm ci)..."
  npm ci
else
  echo "⏭️  Skipping npm ci (reusing node_modules)."
fi

echo "🔨 Building extension (npm run build)..."
npm run build

echo "📦 Packaging extension (npm run package)..."
npm run package

# vsce writes <name>-<version>.vsix; pick the freshest one.
VSIX="$(ls -t ./*.vsix 2>/dev/null | head -n1 || true)"
if [ -z "$VSIX" ]; then
  echo "❌ No .vsix produced in $EXT_DIR." >&2
  exit 1
fi

echo "🚀 Installing $VSIX into '$EDITOR_BIN'..."
"$EDITOR_BIN" --install-extension "$VSIX" --force

echo "✅ Installed $(basename "$VSIX"). Reload the editor window to activate it."
