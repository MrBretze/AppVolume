#!/usr/bin/env bash
# Build the plugin for this machine and install it into OpenDeck (Flatpak or native).
# Usage: scripts/install.sh [--debug]
set -euo pipefail

cd "$(dirname "$0")/.."

PLUGIN_ID="com.mrbretzel.appvolume.sdPlugin"
HOST="$(rustc -vV | sed -n 's/^host: //p')"
TARGET="$HOST"
# On Linux, build a static musl binary: the OpenDeck Flatpak runtime may ship an older
# glibc than the host. It is still installed under the -linux-gnu name the manifest uses.
if [[ "$HOST" == *-unknown-linux-gnu ]]; then
	TARGET="${HOST%-gnu}-musl"
	rustup target add "$TARGET" >/dev/null 2>&1 || true
fi

if [[ "${1:-}" == "--debug" ]]; then
	cargo build --target "$TARGET"
	BINARY="target/$TARGET/debug/appvolume"
else
	cargo build --release --target "$TARGET"
	BINARY="target/$TARGET/release/appvolume"
fi

# Assemble the plugin folder: assets plus the binary under the name the manifest expects.
STAGE="dist/$PLUGIN_ID"
rm -rf "$STAGE"
mkdir -p "$STAGE"
cp -r assets/. "$STAGE/"
cp "$BINARY" "$STAGE/appvolume-$HOST"
# OpenDeck reads manifest translations from <plugin>/<language>.json, not from lang/.
cp assets/lang/*.json "$STAGE/"

FLATPAK_DIR="$HOME/.var/app/me.amankhanna.opendeck/config/opendeck/plugins"
NATIVE_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/opendeck/plugins"
if [[ -d "$FLATPAK_DIR" ]]; then
	DEST="$FLATPAK_DIR"
elif [[ -d "$NATIVE_DIR" ]]; then
	DEST="$NATIVE_DIR"
else
	echo "OpenDeck plugin folder not found; the plugin is staged in $STAGE" >&2
	exit 1
fi

rm -rf "${DEST:?}/$PLUGIN_ID"
cp -r "$STAGE" "$DEST/"
echo "Installed to $DEST/$PLUGIN_ID — restart OpenDeck to load it."
