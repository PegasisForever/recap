#!/usr/bin/env bash
# Build Recap as a Flatpak and update the static OSTree repo that gets served
# over HTTP. Run it from a machine with flatpak-builder and the GNOME 50 SDK.
#
#   ./packaging/publish.sh              build and update ./ostree-repo
#   PUBLISH_URL=https://example.com/ ./packaging/publish.sh
#
# The repo is a plain directory of files. Copy it to any static host. There is
# no Flathub involved and no review step.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(dirname "$HERE")"

APP_ID=site.pegasis.Recap
REPO="${REPO:-$ROOT/ostree-repo}"
BUILD_DIR="${BUILD_DIR:-$ROOT/.flatpak-build}"
GNUPGHOME="${GNUPGHOME:-$HOME/.local/share/recap-gpg}"
PUBLISH_URL="${PUBLISH_URL:-https://pegasisforever.github.io/recap/}"
export GNUPGHOME

need() { command -v "$1" >/dev/null || { echo "missing: $1" >&2; exit 1; }; }
need flatpak
need flatpak-builder
need ostree
need gpg

# One signing key, created on first run and kept afterwards. The public half
# is embedded in the .flatpakrepo file, so installing needs no extra flags.
if ! gpg --list-keys recap-repo >/dev/null 2>&1; then
  echo "==> creating a signing key in $GNUPGHOME"
  mkdir -p "$GNUPGHOME"; chmod 700 "$GNUPGHOME"
  gpg --batch --generate-key <<'KEY'
%no-protection
Key-Type: eddsa
Key-Curve: ed25519
Name-Real: recap-repo
Name-Comment: Recap Flatpak signing key
Expire-Date: 0
%commit
KEY
fi
KEYID="$(gpg --list-keys --with-colons recap-repo | awk -F: '/^fpr:/{print $10; exit}')"
echo "==> signing key $KEYID"

echo "==> building"
flatpak-builder --user --force-clean \
  --install-deps-from=flathub \
  --repo="$REPO" \
  --gpg-sign="$KEYID" --gpg-homedir="$GNUPGHOME" \
  "$BUILD_DIR" "$HERE/$APP_ID.yml"

# flatpak-builder always exports a .Debug ref and there is no flag to skip it.
# It is half the repo and nobody installing this needs it.
if ostree --repo="$REPO" refs | grep -q "^runtime/$APP_ID.Debug/"; then
  echo "==> dropping the debug ref"
  ostree --repo="$REPO" refs --delete "runtime/$APP_ID.Debug/x86_64/master"
fi

echo "==> updating summary and deltas"
flatpak build-update-repo \
  --prune --prune-depth=1 --generate-static-deltas \
  --title="Recap" --default-branch=master \
  --gpg-sign="$KEYID" --gpg-homedir="$GNUPGHOME" \
  "$REPO"

gpg --export "$KEYID" > "$REPO/recap-repo.gpg"
cat > "$REPO/recap.flatpakrepo" <<EOF
[Flatpak Repo]
Title=Recap
Url=$PUBLISH_URL
Homepage=https://github.com/PegasisForever/recap
Comment=Record your monitors and get one link back
Description=Personal Flatpak remote for Recap. Not affiliated with Flathub.
DefaultBranch=master
GPGKey=$(base64 -w0 < "$REPO/recap-repo.gpg")
EOF

# GitHub Pages runs Jekyll by default, which drops directories starting with
# an underscore and can rewrite files. OSTree needs the bytes served verbatim.
touch "$REPO/.nojekyll"

echo
echo "repo:  $REPO  ($(du -sh "$REPO" | cut -f1))"
echo "serve it at $PUBLISH_URL, then on any machine:"
echo
echo "  flatpak remote-add --user --if-not-exists recap ${PUBLISH_URL}recap.flatpakrepo"
echo "  flatpak install --user recap $APP_ID"
