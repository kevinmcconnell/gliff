#!/usr/bin/env bash
# Build and test the macOS side of gliff on a remote Mac.
#
# Syncs the working tree (uncommitted changes included) to the Mac, then
# runs a command there in the synced checkout. With no command it runs the
# default check: the platform-neutral Rust crates' tests, plus the Swift
# package's build and tests once macos/ exists.
#
#   scripts/mac-build.sh                         # default check
#   scripts/mac-build.sh cargo test -p gliff-proto
#   GLIFF_MAC=other-mac scripts/mac-build.sh
#
# The Mac needs Homebrew with `rust` (and later `cbindgen`), and the Xcode
# Command Line Tools. Tests use swift-testing, since the Command Line Tools
# have no XCTest. An open ssh master at ~/.ssh/cm-<host> is reused, so a
# 1Password-backed key is only approved once.
set -euo pipefail
cd "$(dirname "$0")/.."

HOST=${GLIFF_MAC:-nyc-m4}
DIR=${GLIFF_MAC_DIR:-src/gliff}
SSH=(ssh)
if [ -S "$HOME/.ssh/cm-$HOST" ]; then
    SSH=(ssh -o "ControlPath=$HOME/.ssh/cm-$HOST")
fi

"${SSH[@]}" "$HOST" "mkdir -p '$DIR'"
rsync -a --delete --exclude /target/ --exclude /macos/.build/ --exclude /dist/ \
    -e "${SSH[*]}" ./ "$HOST:$DIR/"

if [ $# -eq 0 ]; then
    cmd='cargo test --locked -p gliff-proto -p gliff-transport
         if [ -f macos/Package.swift ]; then (cd macos && swift build && swift test); fi'
else
    cmd=$(printf '%q ' "$@")
fi

"${SSH[@]}" "$HOST" "eval \"\$(/opt/homebrew/bin/brew shellenv)\" && cd '$DIR' && set -e && $cmd"
