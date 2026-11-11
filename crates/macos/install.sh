#!/bin/bash
# Build and install the screentime client on a Mac, from a checkout of the repository.
#
#   crates/macos/install.sh --server https://benwbooth-nixos.duckdns.org:8443 --token TOKEN --users bbooth=adrian
#
# With sudo it installs system-wide: a root LaunchDaemon (the limited account can't stop it) and
# the menu-bar app for everyone. Without sudo it installs for the current account only, as
# LaunchAgents in that account; that's only sensible when the limited account is an
# administrator anyway (it could stop a daemon too).
#
# --users takes local accounts, with =kid when the server knows them by another name.
# --fingerprint is only needed for a self-signed server certificate.
# Run it again to upgrade or change settings; usage and settings are kept.
set -euo pipefail

server="" token="" users="" fingerprint=""
while [ $# -gt 0 ]; do
  case "$1" in
    --server) server="$2"; shift 2 ;;
    --token) token="$2"; shift 2 ;;
    --users) users="$2"; shift 2 ;;
    --fingerprint) fingerprint="$2"; shift 2 ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
  esac
done

root="$(cd "$(dirname "$0")/../.." && pwd)"
label_agent=org.screentime.agent
label_menu=org.screentime.menu

if [ "$(id -u)" = 0 ]; then
  system=1
  data="/Library/Application Support/screentime"
  bin="/Library/Application Support/screentime/bin"
  build_user="${SUDO_USER:-root}"
else
  system=0
  data="$HOME/Library/Application Support/screentime"
  bin="$data/bin"
  build_user="$(id -un)"
fi

# Keep the token and settings from the last install unless new ones were given.
if [ -z "$token" ] && [ -f "$data/token" ]; then token="$(cat "$data/token")"; fi
if [ -f "$data/config.env" ]; then
  [ -n "$server" ] || server="$(sed -n 's/^SCREENTIME_SERVER=//p' "$data/config.env")"
  [ -n "$users" ] || users="$(sed -n 's/^SCREENTIME_USERS=//p' "$data/config.env")"
  [ -n "$fingerprint" ] || fingerprint="$(sed -n 's/^SCREENTIME_SERVER_FINGERPRINT=//p' "$data/config.env")"
fi
if [ -z "$server" ] || [ -z "$token" ] || [ -z "$users" ]; then
  echo "need --server, --token and --users the first time" >&2
  exit 2
fi

echo "Building..."
run_as_builder() { if [ "$system" = 1 ] && [ "$build_user" != root ]; then sudo -u "$build_user" -H "$@"; else "$@"; fi; }
cargo="$(run_as_builder bash -lc 'command -v cargo || echo ~/.cargo/bin/cargo')"
run_as_builder "$cargo" build --release --manifest-path "$root/Cargo.toml" -p screentime-macos
out="$root/target/macos-dist"
run_as_builder mkdir -p "$out/ScreenTimeMenu.app/Contents/MacOS"
run_as_builder swiftc -O -o "$out/ScreenTimeMenu.app/Contents/MacOS/ScreenTimeMenu" \
  "$root/crates/macos/menubar/ScreenTimeMenu.swift" -framework AppKit
run_as_builder tee "$out/ScreenTimeMenu.app/Contents/Info.plist" >/dev/null <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleIdentifier</key><string>$label_menu</string>
  <key>CFBundleName</key><string>Screen time</string>
  <key>CFBundleExecutable</key><string>ScreenTimeMenu</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>1.0</string>
  <key>LSUIElement</key><true/>
</dict></plist>
EOF
run_as_builder codesign --force -s - "$out/ScreenTimeMenu.app"
# The agent goes in an app bundle too: macOS only lets an app with a bundle identity and a
# reason ask for Local Network access, and without it the server on the LAN is "no route to host".
agent_app="$out/ScreenTimeAgent.app"
run_as_builder mkdir -p "$agent_app/Contents/MacOS"
run_as_builder cp -f "$root/target/release/screentime-mac" "$agent_app/Contents/MacOS/"
run_as_builder tee "$agent_app/Contents/Info.plist" >/dev/null <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleIdentifier</key><string>$label_agent</string>
  <key>CFBundleName</key><string>Screen time</string>
  <key>CFBundleExecutable</key><string>screentime-mac</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>1.0</string>
  <key>LSUIElement</key><true/>
  <key>LSBackgroundOnly</key><true/>
  <key>NSLocalNetworkUsageDescription</key><string>Screen time talks to the family's Screen time server on the home network.</string>
</dict></plist>
EOF
run_as_builder codesign --force -s - "$agent_app"

echo "Installing into $data..."
uid_gui=""
stop() { # label domain plist
  launchctl bootout "$2/$1" 2>/dev/null || true
}
if [ "$system" = 1 ]; then
  stop $label_agent system
  for u in $(who | awk '$2=="console"{print $1}' | sort -u); do stop $label_menu "gui/$(id -u "$u")"; done
else
  uid_gui="$(id -u)"
  stop $label_agent "gui/$uid_gui"
  stop $label_menu "gui/$uid_gui"
fi
pkill -x ScreenTimeMenu 2>/dev/null || true

mkdir -p "$bin"
rm -rf "$bin/ScreenTimeMenu.app" "$bin/ScreenTimeAgent.app" "$bin/screentime-mac"
cp -R "$out/ScreenTimeMenu.app" "$out/ScreenTimeAgent.app" "$bin/"
printf '%s' "$token" > "$data/token"
{
  echo "SCREENTIME_SERVER=$server"
  echo "SCREENTIME_TOKEN_FILE=$data/token"
  echo "SCREENTIME_USERS=$users"
  echo "SCREENTIME_SIGN_OUT=false"
  [ -z "$fingerprint" ] || echo "SCREENTIME_SERVER_FINGERPRINT=$fingerprint"
} > "$data/config.env"
chmod 600 "$data/token" "$data/config.env"

plist() { # label program args... -> plist on stdout
  local label="$1"; shift
  echo '<?xml version="1.0" encoding="UTF-8"?>'
  echo '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">'
  echo "<plist version=\"1.0\"><dict>"
  echo "  <key>Label</key><string>$label</string>"
  echo "  <key>ProgramArguments</key><array>"
  for a in "$@"; do echo "    <string>$a</string>"; done
  echo "  </array>"
  echo "  <key>RunAtLoad</key><true/>"
  echo "  <key>KeepAlive</key><true/>"
  echo "  <key>ProcessType</key><string>Interactive</string>"
}
log="$data/agent.log"
if [ "$system" = 1 ]; then
  chown -R root:wheel "$data"
  # Others may reach the socket inside, but not list or read anything.
  chmod 711 "$data"
  { plist $label_agent "$bin/ScreenTimeAgent.app/Contents/MacOS/screentime-mac"
    echo "  <key>StandardErrorPath</key><string>$log</string>"
    echo "</dict></plist>"; } > /Library/LaunchDaemons/$label_agent.plist
  { plist $label_menu "$bin/ScreenTimeMenu.app/Contents/MacOS/ScreenTimeMenu"
    echo "  <key>LimitLoadToSessionType</key><string>Aqua</string>"
    echo "</dict></plist>"; } > /Library/LaunchAgents/$label_menu.plist
  chmod 644 /Library/LaunchDaemons/$label_agent.plist /Library/LaunchAgents/$label_menu.plist
  launchctl bootstrap system /Library/LaunchDaemons/$label_agent.plist
  for u in $(who | awk '$2=="console"{print $1}' | sort -u); do
    launchctl bootstrap "gui/$(id -u "$u")" /Library/LaunchAgents/$label_menu.plist || true
  done
else
  chmod 700 "$data"
  agents="$HOME/Library/LaunchAgents"
  mkdir -p "$agents"
  { plist $label_agent "$bin/ScreenTimeAgent.app/Contents/MacOS/screentime-mac"
    echo "  <key>StandardErrorPath</key><string>$log</string>"
    echo "  <key>LimitLoadToSessionType</key><string>Aqua</string>"
    echo "</dict></plist>"; } > "$agents/$label_agent.plist"
  { plist $label_menu "$bin/ScreenTimeMenu.app/Contents/MacOS/ScreenTimeMenu"
    echo "  <key>LimitLoadToSessionType</key><string>Aqua</string>"
    echo "</dict></plist>"; } > "$agents/$label_menu.plist"
  launchctl bootstrap "gui/$uid_gui" "$agents/$label_agent.plist"
  launchctl bootstrap "gui/$uid_gui" "$agents/$label_menu.plist"
fi

echo "Installed. Limited accounts: $users"
echo "Log: $log"
echo "Preview the warning: '$bin/ScreenTimeMenu.app/Contents/MacOS/ScreenTimeMenu' --preview"
