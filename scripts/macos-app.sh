#!/bin/sh
# Builds the viewer and makes a macOS app for it: PsConnector.app with the app icon, the local network
# permission text and a launcher that runs this repository's release build (so a later
# `cargo build --release` is picked up and macros come from this repository's macros folder).
#
#   scripts/macos-app.sh                 # makes ~/Desktop/PsConnector.app
#   scripts/macos-app.sh /Applications   # or in another folder
#
# Logs go to ~/Library/Logs/psconnector-view.log.
set -eu
repo=$(cd "$(dirname "$0")/.." && pwd)
dest=${1:-"$HOME/Desktop"}
app="$dest/PsConnector.app"

cargo build --release -p psconnector-view --manifest-path "$repo/Cargo.toml"

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$repo/crates/psconnector-view/assets/icon.icns" "$app/Contents/Resources/PsConnector.icns"

cat > "$app/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleDisplayName</key>
	<string>PsConnector</string>
	<key>CFBundleExecutable</key>
	<string>PsConnector</string>
	<key>CFBundleIconFile</key>
	<string>PsConnector</string>
	<key>CFBundleIdentifier</key>
	<string>local.psconnector.view</string>
	<key>CFBundleName</key>
	<string>PsConnector</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleVersion</key>
	<string>1</string>
	<key>NSHighResolutionCapable</key>
	<true/>
	<key>NSLocalNetworkUsageDescription</key>
	<string>Connects to the PS5 on this network.</string>
</dict>
</plist>
PLIST

# Finder starts apps with a bare PATH; the launcher takes the login shell's so macros find python3, uv etc.
# No address is passed: the viewer finds the console used last time by its id.
cat > "$app/Contents/MacOS/PsConnector" <<LAUNCHER
#!/bin/zsh
export PATH="\$(/bin/zsh -lc 'print -r -- \$PATH' 2>/dev/null || echo /usr/bin:/bin)"
exec "$repo/target/release/psconnector-view" >"\$HOME/Library/Logs/psconnector-view.log" 2>&1
LAUNCHER
chmod +x "$app/Contents/MacOS/PsConnector"

# Finder caches icons: touching the bundle makes it read the new one
touch "$app"
echo "made $app"
