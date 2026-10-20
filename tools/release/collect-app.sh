#!/usr/bin/env bash
# Copies the app bundles Tauri built into dist/ under stable names, so links
# like releases/latest/download/pubky-music-linux-x86_64.AppImage keep working:
#   pubky-music-<platform>.{dmg,msi,AppImage,deb} and pubky-music-<platform>-setup.exe
# Usage: collect-app.sh <platform> (for example linux-x86_64, macos-universal)
set -euo pipefail
platform="$1"
cd "$(dirname "$0")/../.."
mkdir -p dist
found=0
while IFS= read -r -d '' f; do
  case "$f" in
    *.dmg) out="pubky-music-$platform.dmg" ;;
    *.msi) out="pubky-music-$platform.msi" ;;
    *-setup.exe) out="pubky-music-$platform-setup.exe" ;;
    *.AppImage) out="pubky-music-$platform.AppImage" ;;
    *.deb) out="pubky-music-$platform.deb" ;;
    *) continue ;;
  esac
  cp "$f" "dist/$out"
  echo "$f -> dist/$out"
  found=$((found + 1))
done < <(find crates/pm-desktop/target -path '*/release/bundle/*' -type f \
  \( -name '*.dmg' -o -name '*.msi' -o -name '*-setup.exe' -o -name '*.AppImage' -o -name '*.deb' \) -print0)
if [ "$found" -eq 0 ]; then
  echo "no bundles found under crates/pm-desktop/target/**/release/bundle" >&2
  exit 1
fi
