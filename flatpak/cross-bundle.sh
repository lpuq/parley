#!/usr/bin/env bash
# Cross-build an x86_64 Parley Flatpak bundle on an aarch64 machine (no emulation):
# the official Rust toolchain from the Flatpak SDK extension plus the host's clang
# target x86_64, linking against the x86_64 GNOME SDK as sysroot.
#   flatpak/cross-bundle.sh     -> parley-x86_64.flatpak
# Build machine needs: flatpak, clang, lld, llvm (llvm-ar), pkgconf, curl.
set -euo pipefail
cd "$(dirname "$0")/.."

APP_ID=io.github.parley.Parley
TARGET=x86_64-unknown-linux-gnu
CACHE="${XDG_CACHE_HOME:-$HOME/.cache}/parley-build"
C="$CACHE/cross"
mkdir -p "$C/cargo" "$C/target" "$C/dl"

command -v clang >/dev/null && command -v ld.lld >/dev/null && command -v llvm-ar >/dev/null ||
  { echo "Needs clang, lld and llvm on the build machine (sudo dnf install clang lld llvm)."; exit 1; }

flatpak info --arch=aarch64 org.freedesktop.Sdk.Extension.rust-stable//26.08 >/dev/null 2>&1 ||
  flatpak install --user -y --noninteractive --arch=aarch64 flathub org.freedesktop.Sdk.Extension.rust-stable//26.08
for ref in org.gnome.Platform//51 org.gnome.Sdk//51; do
  flatpak info --arch=x86_64 "$ref" >/dev/null 2>&1 || flatpak install --user -y --noninteractive --arch=x86_64 flathub "$ref"
done
X86_SDK="$(flatpak info --show-location --arch=x86_64 org.gnome.Sdk//51)/files"

# The extension's official Rust binaries run fine on the host; pair them with the
# matching official x86_64 standard library (checksum-verified).
RUST_BIN="$(flatpak info --show-location --arch=aarch64 org.freedesktop.Sdk.Extension.rust-stable//26.08)/files/bin"
RUSTVER=$("$RUST_BIN/rustc" --version | awk '{print $2}')
STD="$C/rust-std-$RUSTVER"
if [[ ! -d "$STD/lib/rustlib/$TARGET" ]]; then
  tarball="rust-std-$RUSTVER-$TARGET.tar.xz"
  ( cd "$C/dl" && curl -sSfLO "https://static.rust-lang.org/dist/$tarball" \
      && curl -sSfLO "https://static.rust-lang.org/dist/$tarball.sha256" && sha256sum -c "$tarball.sha256" )
  rm -rf "$STD.tmp" && mkdir -p "$STD.tmp"
  tar -xJf "$C/dl/$tarball" -C "$STD.tmp"
  mkdir -p "$STD/lib/rustlib"
  mv "$STD.tmp/rust-std-$RUSTVER-$TARGET/rust-std-$TARGET/lib/rustlib/$TARGET" "$STD/lib/rustlib/"
  rm -rf "$STD.tmp"
fi

# Sysroot shaped like a running runtime: usr = the x86_64 SDK, plus lib/lib64 links.
ROOT="$C/x86root"
rm -rf "$ROOT" && mkdir -p "$ROOT"   # only symlinks inside
ln -sfn "$X86_SDK" "$ROOT/usr"
ln -sfn usr/lib "$ROOT/lib"
ln -sfn usr/lib64 "$ROOT/lib64"

# Host clang (supports every target) as C compiler and linker for x86_64.
printf '#!/bin/sh\nexec clang --target=%s --sysroot=%s -fuse-ld=lld "$@"\n' "$TARGET" "$ROOT" > "$C/x86-cc"
chmod +x "$C/x86-cc"

echo "==> Cross-compiling for x86_64 (Rust $RUSTVER)"
env PATH="$RUST_BIN:$PATH" \
  CARGO_HOME="$C/cargo" CARGO_TARGET_DIR="$C/target" \
  CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$C/x86-cc" \
  CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="--sysroot=$STD" \
  CC_x86_64_unknown_linux_gnu="$C/x86-cc" AR_x86_64_unknown_linux_gnu=llvm-ar \
  PKG_CONFIG_ALLOW_CROSS=1 PKG_CONFIG_SYSROOT_DIR="$ROOT" \
  PKG_CONFIG_LIBDIR="$ROOT/usr/lib/x86_64-linux-gnu/pkgconfig:$ROOT/usr/share/pkgconfig" \
  BINDGEN_EXTRA_CLANG_ARGS_x86_64_unknown_linux_gnu="--target=$TARGET --sysroot=$ROOT" \
  cargo build --release --locked --target "$TARGET"

# Package (no x86 code runs here).
APPDIR="$C/app" REPO="$C/repo"
rm -rf "$APPDIR"
flatpak build-init --arch=x86_64 "$APPDIR" "$APP_ID" org.gnome.Sdk org.gnome.Platform 51 >/dev/null
install -Dm755 "$C/target/$TARGET/release/parley" "$APPDIR/files/bin/parley"
install -Dm644 data/$APP_ID.desktop "$APPDIR/files/share/applications/$APP_ID.desktop"
install -Dm644 data/icons/$APP_ID.svg "$APPDIR/files/share/icons/hicolor/scalable/apps/$APP_ID.svg"
install -Dm644 data/$APP_ID.metainfo.xml "$APPDIR/files/share/metainfo/$APP_ID.metainfo.xml"
mapfile -t PERMS < <(sed -n '/^finish-args:/,/^[a-z]/p' flatpak/$APP_ID.yml | sed -n 's/^  - \(--[^ ]*\).*/\1/p')
flatpak build-finish "$APPDIR" --command=parley "${PERMS[@]}" >/dev/null
flatpak build-export --arch=x86_64 "$REPO" "$APPDIR" master >/dev/null
flatpak build-bundle --arch=x86_64 "$REPO" parley-x86_64.flatpak "$APP_ID" \
  --runtime-repo=https://dl.flathub.org/repo/flathub.flatpakrepo
echo "==> Wrote parley-x86_64.flatpak"
