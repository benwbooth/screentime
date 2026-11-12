#!/bin/bash
# Build the Mac client's two app bundles into target/macos-dist (or $OUT):
#   ScreenTimeAgent.app  the agent (in a bundle so macOS lets it ask for Local Network access;
#                        without it the server on the LAN is "no route to host")
#   ScreenTimeMenu.app   the menu-bar app with the time-left warning
# --universal builds for both Apple silicon and Intel (for release downloads).
# install.sh runs this; set CARGO to pick a cargo other than the one on PATH.
set -euo pipefail
universal=0
[ "${1:-}" = --universal ] && universal=1
root="$(cd "$(dirname "$0")/../.." && pwd)"
out="${OUT:-$root/target/macos-dist}"
cargo="${CARGO:-$(command -v cargo || echo ~/.cargo/bin/cargo)}"
version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)"
label_agent=org.screentime.agent
label_menu=org.screentime.menu

rm -rf "$out/ScreenTimeMenu.app" "$out/ScreenTimeAgent.app"
mkdir -p "$out/ScreenTimeMenu.app/Contents/MacOS" "$out/ScreenTimeAgent.app/Contents/MacOS"
swift_src="$root/crates/macos/menubar/ScreenTimeMenu.swift"
if [ "$universal" = 1 ]; then
  for t in aarch64-apple-darwin x86_64-apple-darwin; do
    "$cargo" build --release --manifest-path "$root/Cargo.toml" -p screentime-macos --target "$t"
  done
  lipo -create -output "$out/ScreenTimeAgent.app/Contents/MacOS/screentime-mac" \
    "$root"/target/{aarch64,x86_64}-apple-darwin/release/screentime-mac
  for a in arm64 x86_64; do
    swiftc -O -target "$a-apple-macos11" -o "$out/ScreenTimeMenu-$a" "$swift_src" -framework AppKit
  done
  lipo -create -output "$out/ScreenTimeMenu.app/Contents/MacOS/ScreenTimeMenu" "$out"/ScreenTimeMenu-{arm64,x86_64}
  rm -f "$out"/ScreenTimeMenu-{arm64,x86_64}
else
  "$cargo" build --release --manifest-path "$root/Cargo.toml" -p screentime-macos
  cp -f "$root/target/release/screentime-mac" "$out/ScreenTimeAgent.app/Contents/MacOS/"
  swiftc -O -o "$out/ScreenTimeMenu.app/Contents/MacOS/ScreenTimeMenu" "$swift_src" -framework AppKit
fi

cat > "$out/ScreenTimeMenu.app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleIdentifier</key><string>$label_menu</string>
  <key>CFBundleName</key><string>Screen time</string>
  <key>CFBundleExecutable</key><string>ScreenTimeMenu</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>LSUIElement</key><true/>
</dict></plist>
PLIST
cat > "$out/ScreenTimeAgent.app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleIdentifier</key><string>$label_agent</string>
  <key>CFBundleName</key><string>Screen time</string>
  <key>CFBundleExecutable</key><string>screentime-mac</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>LSUIElement</key><true/>
  <key>LSBackgroundOnly</key><true/>
  <key>NSLocalNetworkUsageDescription</key><string>Screen time talks to the family's Screen time server on the home network.</string>
</dict></plist>
PLIST
codesign --force -s - "$out/ScreenTimeMenu.app"
codesign --force -s - "$out/ScreenTimeAgent.app"
echo "$out"
