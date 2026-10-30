#!/usr/bin/env sh
# Runs the Windows app in dev mode from WSL against a freshly built WSL helper.
#
# The app is cross-compiled with cargo-xwin and started as a Windows exe through WSL
# interop. Windows exes only see environment variables listed in WSLENV, so the
# variables below are added there. REFRACT_AGENT and REFRACT_VSIX are Linux paths the
# app passes back into WSL untranslated; REFRACT_WSL_DISTRO makes the app use this
# distro rather than choosing one from `wsl.exe --list`.
#
# `npm run tauri:dev` packages vscode-extension/refract.vsix first if it is missing
# (app/scripts/ensure-vsix.mjs), since the app bundles it and it is not committed.
set -eu

repo=$(cd "$(dirname "$0")/.." && pwd)
export PATH="$HOME/.local/bin:$PATH"

cargo build --manifest-path "$repo/Cargo.toml" -p refract-agent

export REFRACT_AGENT="$repo/target/debug/refract-agent"
export REFRACT_VSIX="$repo/vscode-extension/refract.vsix"
export REFRACT_WSL_DISTRO="${REFRACT_WSL_DISTRO:-${WSL_DISTRO_NAME:-}}"
export WSLENV="REFRACT_AGENT:REFRACT_VSIX:REFRACT_WSL_DISTRO${WSLENV:+:$WSLENV}"

cd "$repo/app"
exec npm run tauri:dev
