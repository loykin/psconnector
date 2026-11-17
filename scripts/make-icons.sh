#!/bin/sh
# Regenerates every app icon file from the drawing in crates/psconnector-view/assets/appicon.swift:
#   assets/icon.png   256×256, the window / Dock / taskbar icon embedded in the viewer
#   assets/icon.icns  the macOS app bundle icon (scripts/macos-app.sh)
#   assets/icon.ico   the Windows .exe icon (embedded by build.rs)
# Run it on macOS after changing appicon.swift, and commit the results; builds only use the files.
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
assets="$here/crates/psconnector-view/assets"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

swift "$assets/appicon.swift" "$work/icon1024.png" 1024

# window icon
sips -z 256 256 "$work/icon1024.png" --out "$assets/icon.png" >/dev/null

# macOS .icns
set_dir="$work/icon.iconset"
mkdir -p "$set_dir"
for s in 16 32 128 256 512; do
    sips -z $s $s "$work/icon1024.png" --out "$set_dir/icon_${s}x${s}.png" >/dev/null
    d=$((s * 2))
    sips -z $d $d "$work/icon1024.png" --out "$set_dir/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$set_dir" -o "$assets/icon.icns"

# Windows .ico: PNG images inside an ICO container (Windows Vista and later read these)
for s in 16 24 32 48 64 128 256; do
    sips -z $s $s "$work/icon1024.png" --out "$work/ico_$s.png" >/dev/null
done
python3 - "$assets/icon.ico" "$work" <<'PY'
import struct, sys
out, work = sys.argv[1], sys.argv[2]
sizes = [16, 24, 32, 48, 64, 128, 256]
images = [open(f"{work}/ico_{s}.png", "rb").read() for s in sizes]
header = struct.pack("<HHH", 0, 1, len(images))
offset = 6 + 16 * len(images)
entries = b""
for s, data in zip(sizes, images):
    entries += struct.pack("<BBBBHHII", s % 256, s % 256, 0, 0, 1, 32, len(data), offset)
    offset += len(data)
open(out, "wb").write(header + entries + b"".join(images))
PY

echo "icons written to $assets: icon.png, icon.icns, icon.ico"
