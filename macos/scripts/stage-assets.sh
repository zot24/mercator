#!/bin/sh
# Re-running replaces the binary and the dashboard copy.
set -eu
root=$(CDPATH= cd -- "$(dirname "$0")/../.." && pwd)
mkdir -p "$root/macos/assets"
cp "$root/target/release/mercator" "$root/macos/assets/mercator"
chmod +x "$root/macos/assets/mercator"
rm -rf "$root/macos/assets/dist"
cp -R "$root/dist" "$root/macos/assets/dist"
