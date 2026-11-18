#!/bin/bash
# Builds Parley for Windows (x64) and gathers it, with everything it needs, into
# dist/Parley, laid out the way GTK and GStreamer look for their files on Windows:
#   bin\parley.exe and the DLLs it (and its plugins) use
#   lib\gstreamer-1.0\   the GStreamer plugins Parley uses, libexec\…\gst-plugin-scanner.exe
#   lib\gdk-pixbuf-2.0\  image loaders; share\icons\ (Adwaita, Parley's icon); share\glib-2.0\schemas\
# then zips it as dist/Parley-<version>-windows-x64.zip.
# Run in an MSYS2 CLANG64 shell with the packages from the README installed.
set -euo pipefail
cd "$(dirname "$0")/.."
P=${MINGW_PREFIX:-/clang64}
VERSION=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
OUT=dist/Parley

cargo build --release

rm -rf dist
mkdir -p "$OUT/bin" "$OUT/lib/gstreamer-1.0" "$OUT/libexec/gstreamer-1.0" "$OUT/share/glib-2.0/schemas" "$OUT/share/icons/hicolor/scalable/apps"
cp target/release/parley.exe "$OUT/bin/"

# The GStreamer plugins behind every element Parley uses (see src/media), plus the
# Windows ones: WASAPI sound, Media Foundation cameras and encoders, Direct3D 11.
PLUGINS="coreelements app audioconvert audiomixer audioresample audiotestsrc autodetect volume
  opus rtp rtpmanager videoconvertscale videorate videoparsersbad debug jpeg
  x264 openh264 libav gtk4 webrtcdsp wasapi wasapi2 mediafoundation d3d11"
for p in $PLUGINS; do
  cp "$P/lib/gstreamer-1.0/libgst$p.dll" "$OUT/lib/gstreamer-1.0/"
done
cp "$P/libexec/gstreamer-1.0/gst-plugin-scanner.exe" "$OUT/libexec/gstreamer-1.0/"

# Image loaders (for pictures people pick as avatars), without the HEIF, JPEG XL and
# WMF ones and the large libraries they'd bring along. MSYS2's cache of them already
# names them by relative paths, so it only needs those three left out.
L=lib/gdk-pixbuf-2.0/2.10.0
mkdir -p "$OUT/$L/loaders"
for f in "$P/$L/loaders/"*.dll; do
  case "$(basename "$f")" in *heif* | *jxl* | io-wmf*) ;; *) cp "$f" "$OUT/$L/loaders/" ;; esac
done
awk 'BEGIN { RS = ""; ORS = "\n\n" } !/heif|jxl|io-wmf/' "$P/$L/loaders.cache" > "$OUT/$L/loaders.cache"

# Every DLL from MSYS2 that any of these needs, directly or not.
# (Paths compared with forward slashes: awk would read the backslashes as escapes.)
prefix_bin=$(cygpath -m "$P/bin" | tr '[:upper:]' '[:lower:]')
mapfile -t binaries < <(find "$OUT" -name '*.exe' -o -name '*.dll')
ntldd -R "${binaries[@]}" | tr -d '\r' | tr '\\' '/' | awk -v pb="$prefix_bin" '
  { if (index(tolower($0), pb) > 0) { sub(/^[^>]*=> */, ""); sub(/ \(0x[0-9a-f]+\)$/, ""); print } }' |
  sort -u | while read -r dll; do cp -n "$(cygpath -u "$dll")" "$OUT/bin/"; done
echo "$(ls "$OUT/bin" | wc -l) files in bin"
# (The plugin scanner finds them too: Parley puts bin\ on PATH for its helpers.)

# GTK's settings schemas, the Adwaita icons it draws its own widgets with, and
# Parley's icon by its name (the window and taskbar use it).
cp "$P/share/glib-2.0/schemas/gschemas.compiled" "$OUT/share/glib-2.0/schemas/"
cp -r "$P/share/icons/Adwaita" "$OUT/share/icons/"
cp "$P/share/icons/hicolor/index.theme" "$OUT/share/icons/hicolor/"
cp data/icons/io.github.parley.Parley.svg "$OUT/share/icons/hicolor/scalable/apps/"

cp LICENSE "$OUT/LICENSE.txt"
cat > "$OUT/README.txt" <<EOF
Parley $VERSION for Windows

Start Parley with bin\\parley.exe.
Your identity, friends and chats are kept in %LOCALAPPDATA%\\parley and %APPDATA%\\parley.

Parley is free software (GPL-3.0-or-later, see LICENSE.txt); it uses GTK, libadwaita
and GStreamer (LGPL), among others. Source: https://github.com/lpuq/parley
EOF

(cd dist && zip -qr "Parley-$VERSION-windows-x64.zip" Parley)
du -sh "$OUT" "dist/Parley-$VERSION-windows-x64.zip"
