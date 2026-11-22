#!/usr/bin/env bash
# Signs the freshly built Parley (aarch64 from ./install.sh --bundle, x86_64 from
# flatpak/cross-bundle.sh) into the hosted Flatpak repo that in-app updates come from,
# and rebuilds the bundles from it, so a bundle install follows that repo from then on.
#   flatpak/publish-repo.sh          update the local copy and the bundles
#   flatpak/publish-repo.sh --push   also publish it (GitHub Pages, branch gh-pages)
# Keys stay in ~/.config/parley-release; only the public key is in this repository.
set -euo pipefail
cd "$(dirname "$0")/.."

APP_ID=io.github.parley.Parley
CACHE="${XDG_CACHE_HOME:-$HOME/.cache}/parley-build"
KEYS="${PARLEY_RELEASE_KEYS:-$HOME/.config/parley-release}"
SITE="${PARLEY_PUBLISH_DIR:-$CACHE/publish/site}"
URL="${PARLEY_REPO_URL:-https://lpuq.github.io/parley/repo}"
ARCHES="${PARLEY_ARCHES:-aarch64 x86_64}"
ORIGIN=https://github.com/lpuq/parley.git
PUSH=0
[[ "${1:-}" == --push ]] && PUSH=1

KEY=$(gpg --homedir "$KEYS/gnupg" --list-secret-keys --with-colons | awk -F: '/^fpr/{print $10; exit}')
[[ -n "$KEY" ]] || { echo "No release key in $KEYS" >&2; exit 1; }
SIGN=(--gpg-sign="$KEY" --gpg-homedir="$KEYS/gnupg")

# Carry on from what's published, so people can update from older versions.
if [[ ! -d "$SITE/repo" ]] && git ls-remote --exit-code --heads "$ORIGIN" gh-pages >/dev/null 2>&1; then
  rm -rf "$SITE"
  git clone -q --depth 1 --branch gh-pages "$ORIGIN" "$SITE"
fi
mkdir -p "$SITE"

for arch in $ARCHES; do
  case "$arch" in
    x86_64) app="${PARLEY_APPDIR_x86_64:-$CACHE/cross/app}" ;;
    *) app="${PARLEY_APPDIR_aarch64:-$CACHE/$arch/app}" ;;
  esac
  [[ -f "$app/metadata" ]] || { echo "No $arch build in $app" >&2; exit 1; }
  flatpak build-export --arch="$arch" "${SIGN[@]}" "$SITE/repo" "$app" master >/dev/null
done
flatpak build-update-repo "${SIGN[@]}" --title=Parley --default-branch=master \
  --generate-static-deltas --prune --prune-depth=4 "$SITE/repo" >/dev/null

for arch in $ARCHES; do
  flatpak build-bundle --arch="$arch" --repo-url="$URL" --gpg-keys=flatpak/parley-repo.gpg \
    --runtime-repo=https://dl.flathub.org/repo/flathub.flatpakrepo \
    "$SITE/repo" "parley-$arch.flatpak" "$APP_ID" master
  echo "Wrote parley-$arch.flatpak"
done

if [[ $PUSH == 1 ]]; then
  # One commit: the repo's own history is what matters, and this keeps the branch small.
  touch "$SITE/.nojekyll"
  printf 'repo/tmp/\nrepo/.lock\n' > "$SITE/.gitignore"
  rm -rf "$SITE/.git"
  git -C "$SITE" init -q -b gh-pages
  git -C "$SITE" add -A
  git -C "$SITE" -c user.name=lpuq -c user.email=254144542+lpuq@users.noreply.github.com commit -q -m "Parley Flatpak repo"
  git -C "$SITE" push -q --force "$ORIGIN" gh-pages
  gh api repos/lpuq/parley/pages >/dev/null 2>&1 ||
    gh api -X POST repos/lpuq/parley/pages -f 'source[branch]=gh-pages' -f 'source[path]=/' >/dev/null
  echo "Published to $URL"
fi
