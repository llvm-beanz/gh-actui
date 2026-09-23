#!/usr/bin/env bash
set -euo pipefail

tag="${1:?release tag is required}"
expected=(
  "dist/gh-actui_${tag}_linux-amd64"
  "dist/gh-actui_${tag}_linux-arm64"
  "dist/gh-actui_${tag}_darwin-amd64"
  "dist/gh-actui_${tag}_darwin-arm64"
  "dist/gh-actui_${tag}_windows-amd64.exe"
  "dist/gh-actui_${tag}_windows-arm64.exe"
)

for asset in "${expected[@]}"; do
  if [[ ! -f "$asset" ]]; then
    echo "error: expected release asset not found: $asset" >&2
    exit 1
  fi
done
