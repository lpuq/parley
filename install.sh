#!/usr/bin/env bash
# Parley installer — one command, no manual setup.
#
#   ./install.sh               install: builds from source in a git checkout; elsewhere uses
#                              a prebuilt parley-<arch>.flatpak next to this script if present
#   ./install.sh --prebuilt    use the prebuilt parley-<arch>.flatpak even in a git checkout
#   ./install.sh --bundle      also write parley-<arch>.flatpak to copy to other machines
#   ./install.sh --native      build a native binary instead of a Flatpak (needs -devel pkgs)
#   ./install.sh --uninstall
#
# The Flatpak route needs only `flatpak` itself (preinstalled on Fedora Workstation,
# offered via your package manager otherwise). Everything else — Flathub, the GNOME
# SDK, Rust, codecs — is fetched automatically into your user account; no sudo.
set -euo pipefail
cd "$(dirname "$0")"

APP_ID=io.github.parley.Parley
RUNTIME_BRANCH=51          # org.gnome.Platform / Sdk
FDO_BRANCH=26.08           # its freedesktop base (for SDK extensions)
MANIFEST=flatpak/$APP_ID.yml
CACHE="${XDG_CACHE_HOME:-$HOME/.cache}/parley-build"
ARCH="${PARLEY_ARCH:-$(flatpak --default-arch 2>/dev/null || uname -m)}"
MODE=auto BUNDLE=0 PREBUILT=0
for a in "$@"; do
  case "$a" in
    --bundle) BUNDLE=1 ;;
    --prebuilt) PREBUILT=1 ;;
    --native) MODE=native ;;
    --uninstall) MODE=uninstall ;;
    -h|--help) sed -n '2,13p' "$0"; exit 0 ;;
    *) echo "Unknown option: $a"; exit 2 ;;
  esac
done

say()  { printf '\033[1m==> %s\033[0m\n' "$*"; }
warn() { printf '\033[33m%s\033[0m\n' "$*" >&2; }
die()  { printf '\033[31mError: %s\033[0m\n' "$*" >&2; exit 1; }
ask()  { [[ -t 0 ]] || return 1; read -r -p "$1 [Y/n] " r; [[ -z "$r" || "$r" =~ ^[Yy] ]]; }

pkg_install() { # offer to install distro packages (the only step that may need sudo)
  local cmd
  if command -v dnf >/dev/null; then cmd="sudo dnf install -y $*"
  elif command -v apt-get >/dev/null; then cmd="sudo apt-get install -y $*"
  elif command -v pacman >/dev/null; then cmd="sudo pacman -S --needed --noconfirm $*"
  elif command -v zypper >/dev/null; then cmd="sudo zypper install -y $*"
  else return 1; fi
  ask "Parley needs: $*. Install now with '$cmd'?" && eval "$cmd"
}

# Native (./install.sh --native) and Flatpak installs keep identity and contacts in
# different places, and a native launcher in ~/.local/share/applications hides the
# Flatpak's. When switching, carry the identity over and clear out the other install.
NATIVE_DATA="${XDG_DATA_HOME:-$HOME/.local/share}/parley"
NATIVE_CONFIG="${XDG_CONFIG_HOME:-$HOME/.config}/parley"
FLATPAK_DATA="$HOME/.var/app/$APP_ID/data/parley"
FLATPAK_CONFIG="$HOME/.var/app/$APP_ID/config/parley"

# copy_identity FROM_DATA FROM_CONFIG TO_DATA TO_CONFIG
copy_identity() {
  if [[ -f "$1/identity.key" && ! -f "$3/identity.key" ]]; then
    mkdir -p "$3" "$4"
    cp -rn "$1/." "$3/"
    [[ -d "$2" ]] && cp -rn "$2/." "$4/"
    say "Moved your identity and contacts over from the other Parley install"
  fi
}

leave_native() {
  local launcher="$HOME/.local/share/applications/$APP_ID.desktop"
  copy_identity "$NATIVE_DATA" "$NATIVE_CONFIG" "$FLATPAK_DATA" "$FLATPAK_CONFIG"
  if [[ -f "$HOME/.local/bin/parley" ]] || { [[ -f "$launcher" ]] && ! grep -q flatpak "$launcher"; }; then
    # Stop a running native Parley (it would hold the app's name and answer for the
    # Flatpak); never the Flatpak one.
    for pid in $(pgrep -x parley); do
      [[ "$(readlink "/proc/$pid/exe")" == "$HOME/.local/bin/parley" ]] && kill "$pid" 2>/dev/null
    done
    rm -f "$HOME/.local/bin/parley" "$launcher" "$HOME/.local/share/icons/hicolor/scalable/apps/$APP_ID.svg"
    say "Removed the old native build so the app menu opens this Flatpak"
  fi
}

# ---------------------------------------------------------------- uninstall
if [[ $MODE == uninstall ]]; then
  flatpak uninstall --user -y "$APP_ID" 2>/dev/null || true
  rm -f "$HOME/.local/bin/parley" "$HOME/.local/share/applications/$APP_ID.desktop" \
        "$HOME/.local/share/icons/hicolor/scalable/apps/$APP_ID.svg" \
        "${XDG_CONFIG_HOME:-$HOME/.config}/autostart/$APP_ID.desktop"
  say "Parley removed. Contacts/identity were kept (~/.var/app/$APP_ID or ~/.config/parley)."
  echo "To also free the build tools (~7 GB): flatpak uninstall --user --unused && rm -rf '$CACHE'"
  exit 0
fi

# ---------------------------------------------------------------- native build
if [[ $MODE == native ]]; then
  if ! pkg-config --exists gtk4 libadwaita-1 gstreamer-1.0 libpipewire-0.3 2>/dev/null || ! command -v cargo >/dev/null; then
    if command -v dnf >/dev/null; then
      pkg_install cargo clang-devel gtk4-devel libadwaita-devel gstreamer1-devel gstreamer1-plugins-base-devel pipewire-devel \
        gstreamer1-plugins-good gstreamer1-plugins-bad-free gstreamer1-plugins-ugly gstreamer1-plugin-libav gstreamer1-plugin-gtk4 || true
    elif command -v apt-get >/dev/null; then
      pkg_install cargo libclang-dev libgtk-4-dev libadwaita-1-dev libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev libpipewire-0.3-dev \
        gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-plugins-ugly gstreamer1.0-libav gstreamer1.0-gtk4 gstreamer1.0-pipewire || true
    elif command -v pacman >/dev/null; then
      pkg_install rust clang gtk4 libadwaita gstreamer gst-plugins-base gst-plugins-good gst-plugins-bad gst-plugins-ugly gst-libav gst-plugin-gtk4 gst-plugin-pipewire pipewire || true
    fi
  fi
  cargo build --release
  copy_identity "$FLATPAK_DATA" "$FLATPAK_CONFIG" "$NATIVE_DATA" "$NATIVE_CONFIG"
  if flatpak info --user "$APP_ID" >/dev/null 2>&1; then
    say "Note: the Flatpak version is installed too. Remove it with: flatpak uninstall --user $APP_ID"
  fi
  install -Dm755 target/release/parley "$HOME/.local/bin/parley"
  install -Dm644 data/$APP_ID.desktop "$HOME/.local/share/applications/$APP_ID.desktop"
  install -Dm644 data/icons/$APP_ID.svg "$HOME/.local/share/icons/hicolor/scalable/apps/$APP_ID.svg"
  say "Installed natively. Launch Parley from your app menu."
  exit 0
fi

# ---------------------------------------------------------------- Flatpak
if ! command -v flatpak >/dev/null; then
  pkg_install flatpak || die "Please install Flatpak (https://flatpak.org/setup/) and run this again."
fi
say "Using Flathub for the GNOME runtime"
flatpak remote-add --user --if-not-exists flathub https://dl.flathub.org/repo/flathub.flatpakrepo

need() { # install a runtime/extension for this user unless any installation has it
  flatpak info --arch="$ARCH" "$1" >/dev/null 2>&1 && return 0
  say "Downloading $1 (one-time)"
  flatpak install --user -y --noninteractive --arch="$ARCH" flathub "$1"
}

finish() {
  leave_native
  say "Installed! Open Parley from your app menu (or: flatpak run $APP_ID)."
  if flatpak ps --columns=application 2>/dev/null | grep -qx "$APP_ID"; then
    # It may be hidden in the background (tray icon) waiting for calls.
    if ask "Parley is still running the old version. Restart it now? (ends any call in progress)"; then
      flatpak kill "$APP_ID" 2>/dev/null || true
      for _ in 1 2 3 4 5 6 7 8 9 10; do
        flatpak ps --columns=application 2>/dev/null | grep -qx "$APP_ID" || break
        sleep 0.5
      done
      setsid flatpak run "$APP_ID" >/dev/null 2>&1 < /dev/null &
      say "Parley restarted with the new version."
    else
      echo "Restart Parley later to use the new version (tray icon → Quit, then open it again)."
    fi
  fi
  exit 0
}

# Fast path: a ready-made package for this machine sits next to the installer.
# Not in a git checkout, though: there a leftover package would silently replace
# the code you just pulled, so we build from source unless asked otherwise.
USE_PREBUILT=0
if [[ -f "parley-$ARCH.flatpak" && $BUNDLE == 0 ]]; then
  if [[ ! -d .git || $PREBUILT == 1 ]]; then
    USE_PREBUILT=1
  else
    say "Building from source (ignoring parley-$ARCH.flatpak here; --prebuilt uses it instead)"
  fi
fi
if [[ $USE_PREBUILT == 1 ]]; then
  need org.gnome.Platform//$RUNTIME_BRANCH
  need org.freedesktop.Platform.codecs-extra//$FDO_BRANCH-extra
  say "Installing prebuilt parley-$ARCH.flatpak"
  flatpak install --user -y --noninteractive --reinstall "parley-$ARCH.flatpak"
  finish
fi

need org.gnome.Platform//$RUNTIME_BRANCH
need org.gnome.Sdk//$RUNTIME_BRANCH
need org.freedesktop.Sdk.Extension.rust-stable//$FDO_BRANCH
need org.freedesktop.Sdk.Extension.llvm22//$FDO_BRANCH
need org.freedesktop.Platform.codecs-extra//$FDO_BRANCH-extra   # H.264 (x264 + libav)

APPDIR="$CACHE/$ARCH/app" REPO="$CACHE/$ARCH/repo"
mkdir -p "$CACHE/$ARCH/cargo" "$CACHE/$ARCH/target"
rm -rf "$APPDIR"
say "Preparing the build sandbox"
flatpak build-init --arch="$ARCH" \
  --sdk-extension=org.freedesktop.Sdk.Extension.rust-stable \
  --sdk-extension=org.freedesktop.Sdk.Extension.llvm22 \
  "$APPDIR" "$APP_ID" org.gnome.Sdk org.gnome.Platform "$RUNTIME_BRANCH"

say "Compiling Parley (first time ~5-10 min; later runs are quick)"
flatpak build --share=network \
  --bind-mount=/run/parley-src="$PWD" --bind-mount=/run/parley-cache="$CACHE/$ARCH" \
  --env=PATH=/usr/lib/sdk/rust-stable/bin:/usr/lib/sdk/llvm22/bin:/app/bin:/usr/bin \
  --env=LIBCLANG_PATH=/usr/lib/sdk/llvm22/lib \
  --env=CARGO_HOME=/run/parley-cache/cargo --env=CARGO_TARGET_DIR=/run/parley-cache/target \
  "$APPDIR" sh -ec '
    cd /run/parley-src
    cargo build --release --locked
    install -Dm755 /run/parley-cache/target/release/parley /app/bin/parley
    install -Dm644 data/'"$APP_ID"'.desktop /app/share/applications/'"$APP_ID"'.desktop
    install -Dm644 data/icons/'"$APP_ID"'.svg /app/share/icons/hicolor/scalable/apps/'"$APP_ID"'.svg
    install -Dm644 data/'"$APP_ID"'.metainfo.xml /app/share/metainfo/'"$APP_ID"'.metainfo.xml
  '

# Sandbox permissions come from the manifest, so the two never drift apart.
mapfile -t PERMS < <(sed -n '/^finish-args:/,/^[a-z]/p' "$MANIFEST" | sed -n 's/^  - \(--[^ ]*\).*/\1/p')
flatpak build-finish "$APPDIR" --command=parley "${PERMS[@]}" >/dev/null
flatpak build-export --arch="$ARCH" "$REPO" "$APPDIR" master >/dev/null

if [[ $BUNDLE == 1 ]]; then
  flatpak build-bundle --arch="$ARCH" "$REPO" "parley-$ARCH.flatpak" "$APP_ID" \
    --runtime-repo=https://dl.flathub.org/repo/flathub.flatpakrepo
  say "Wrote parley-$ARCH.flatpak — copy it next to install.sh on another $ARCH machine and run ./install.sh there."
fi

if [[ "$ARCH" == "$(flatpak --default-arch)" ]]; then
  flatpak remote-add --user --if-not-exists --no-gpg-verify parley-local "$REPO"
  flatpak install --user -y --noninteractive --reinstall parley-local "$APP_ID" >/dev/null
  finish
fi
say "Built for $ARCH (not this machine's architecture), so it wasn't installed here."
