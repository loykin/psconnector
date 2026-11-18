#!/bin/sh
# Makes the macOS app for the viewer: PsConnector.app with the app icon, the local network permission text
# and a launcher that starts the viewer with the login shell's PATH (so macros find python3, uv etc.).
#
#   scripts/macos-app.sh                 # development: ~/Desktop/PsConnector.app runs this repository's
#   scripts/macos-app.sh /Applications   #   release build (a later `cargo build --release` is picked up,
#                                        #   macros come from this repository's macros folder)
#   scripts/macos-app.sh --release target/aarch64-apple-darwin/release/psconnector-view dist
#                                        # release: the viewer is copied into the app, which then uses the
#                                        #   macros folder next to PsConnector.app
#
# The app is signed ad hoc (free, no developer account); a downloaded copy still needs "Open Anyway" once
# (System Settings → Privacy & Security) or `xattr -dr com.apple.quarantine PsConnector.app`.
# Logs go to ~/Library/Logs/psconnector-view.log.
set -eu
repo=$(cd "$(dirname "$0")/.." && pwd)

if [ "${1:-}" = "--release" ]; then
    bin=${2:?usage: macos-app.sh --release <psconnector-view binary> <folder>}
    dest=${3:?usage: macos-app.sh --release <psconnector-view binary> <folder>}
else
    bin=""
    dest=${1:-"$HOME/Desktop"}
    cargo build --release -p psconnector-view --manifest-path "$repo/Cargo.toml"
fi
app="$dest/PsConnector.app"
version=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' "$repo/Cargo.toml")

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$repo/crates/psconnector-view/assets/icon.icns" "$app/Contents/Resources/PsConnector.icns"

cat > "$app/Contents/Info.plist" <<PLIST
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
	<key>CFBundleShortVersionString</key>
	<string>$version</string>
	<key>CFBundleVersion</key>
	<string>$version</string>
	<key>NSHighResolutionCapable</key>
	<true/>
	<key>NSLocalNetworkUsageDescription</key>
	<string>Connects to the PS5 on this network.</string>
</dict>
</plist>
PLIST

# Finder starts apps with a bare PATH; the launcher takes the login shell's so macros find python3, uv etc.
# No address is passed: the viewer finds the console used last time by its id.
if [ -n "$bin" ]; then
    cp "$bin" "$app/Contents/MacOS/psconnector-view"
    target='"${0:A:h}/psconnector-view"'
else
    target="\"$repo/target/release/psconnector-view\""
fi
cat > "$app/Contents/MacOS/PsConnector" <<LAUNCHER
#!/bin/zsh
export PATH="\$(/bin/zsh -lc 'print -r -- \$PATH' 2>/dev/null || echo /usr/bin:/bin)"
exec $target "\$@" >"\$HOME/Library/Logs/psconnector-view.log" 2>&1
LAUNCHER
chmod +x "$app/Contents/MacOS/PsConnector"

if [ -n "$bin" ]; then
    codesign --force --deep --sign - "$app"
fi

# Finder caches icons: touching the bundle makes it read the new one
touch "$app"
echo "made $app"
