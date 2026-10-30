#!/usr/bin/env sh
# Installs the local release build for the current Windows user, so Refract starts
# from the Start menu, a desktop shortcut or at sign-in, with no development
# environment variables. Run it again after rebuilding to update the install.
#
# Build first:
#   cargo build --release -p refract-agent
#   (cd app && npm run tauri:build)
#
# The exe goes to %LOCALAPPDATA%\Programs\Refract with refract-agent and refract.vsix
# beside it, where the app finds its resources. The helper is also copied straight
# into WSL: the app replaces the installed helper only when its version differs, and
# local builds all share one version.
#
# Usage: scripts/install-local.sh [--no-startup]
#   --no-startup  leave start-at-sign-in as it is (it can be changed from the tray)
set -eu

startup=yes
for arg in "$@"; do
    case $arg in
        --no-startup) startup=no ;;
        *) echo "usage: $0 [--no-startup]" >&2; exit 2 ;;
    esac
done

repo=$(cd "$(dirname "$0")/.." && pwd)
exe="$repo/app/src-tauri/target/x86_64-pc-windows-msvc/release/refract.exe"
agent="$repo/target/release/refract-agent"
vsix="$repo/vscode-extension/refract.vsix"
for file in "$exe" "$agent" "$vsix"; do
    if [ ! -s "$file" ]; then
        echo "missing $file; build first (see the top of this script)" >&2
        exit 1
    fi
done

ps() {
    powershell.exe -NoProfile -NonInteractive -Command "$1" | tr -d '\r'
}

win_dir="$(ps '[Environment]::GetFolderPath("LocalApplicationData")')\\Programs\\Refract"
win_exe="$win_dir\\refract.exe"
dir=$(wslpath -u "$win_dir")

# A running copy holds its exe open. Its helper exits when the pipe to it closes.
if tasklist.exe /FI "IMAGENAME eq refract.exe" /NH | grep -qi 'refract\.exe'; then
    echo "stopping the running Refract"
    taskkill.exe /IM refract.exe /F >/dev/null
    sleep 1
fi

mkdir -p "$dir"
cp "$exe" "$agent" "$vsix" "$dir/"

# Replaced by rename, so a helper that is still running keeps its old file.
bin="$HOME/.local/share/refract/bin"
mkdir -p "$bin"
cp "$agent" "$bin/refract-agent.new"
chmod 755 "$bin/refract-agent.new"
mv -f "$bin/refract-agent.new" "$bin/refract-agent"

ps "
\$shell = New-Object -ComObject WScript.Shell
foreach (\$folder in [Environment]::GetFolderPath('Programs'), [Environment]::GetFolderPath('Desktop')) {
    \$link = \$shell.CreateShortcut((Join-Path \$folder 'Refract.lnk'))
    \$link.TargetPath = '$win_exe'
    \$link.WorkingDirectory = '$win_dir'
    \$link.Save()
}
"

if [ "$startup" = yes ]; then
    # The same entry the tray's Start with Windows item writes (tauri-plugin-autostart:
    # value name = product name, data = "<exe> <args>"), so the item shows it as on.
    run='HKCU\Software\Microsoft\Windows\CurrentVersion\Run'
    reg.exe add "$run" /v Refract /t REG_SZ /d "$win_exe --hidden" /f >/dev/null
    # Clears a "disabled" left by Task Manager's Startup apps page, as the plugin does.
    approved='HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run'
    if reg.exe query "$approved" >/dev/null 2>&1; then
        reg.exe add "$approved" /v Refract /t REG_BINARY /d 020000000000000000000000 /f >/dev/null
    fi
fi

# Started from its Windows folder, not through \\wsl.localhost, which can hang.
ps "Start-Process -FilePath '$win_exe' -WorkingDirectory '$win_dir'"

echo "installed to $win_dir"
echo "shortcuts: Start menu and desktop (Refract)"
[ "$startup" = yes ] && echo "starts at sign-in, in the tray"
exit 0
