#!/usr/bin/env bash
# Copies the node's docs into a download archive folder at the same paths as in the repo:
# README.md, docs/deploy.md and docs/guides/, so the README's links to the guides (and the
# guides' links to each other and back) work in the unpacked folder.
# Usage: node-docs.sh <archive folder>
set -euo pipefail
dest="$(mkdir -p "$1" && cd "$1" && pwd)"
cd "$(dirname "$0")/../.."
mkdir -p "$dest/docs"
cp README.md "$dest/"
cp docs/deploy.md "$dest/docs/"
cp -R docs/guides "$dest/docs/"
