#!/usr/bin/env bash
# Signs the unsigned release APK and AAB from `tauri android build`, derives
# an arm64-v8a-only APK from the universal one, and writes dist/:
#   pubky-music-android-universal.apk, pubky-music-android-arm64.apk,
#   pubky-music-android.aab
# Key from the environment (repo secrets): ANDROID_KEYSTORE_B64 (PKCS12),
# ANDROID_KEYSTORE_PASSWORD, ANDROID_KEY_ALIAS, ANDROID_KEY_PASSWORD.
# Without a key the files are written with an -unsigned suffix instead.
set -euo pipefail
cd "$(dirname "$0")/../.."
out=crates/pm-desktop/gen/android/app/build/outputs
apk=$(find "$out/apk" -name '*universal-release*.apk' | head -1)
aab=$(find "$out/bundle" -name '*universal-release*.aab' | head -1)
if [ -z "$apk" ] || [ -z "$aab" ]; then
  echo "android-sign: release outputs missing; found:" >&2
  find "$out" -type f >&2
  exit 1
fi
bt=$(ls -d "$ANDROID_HOME"/build-tools/* | sort -V | tail -1)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p dist

# arm64 only: drop the other ABIs' native libraries.
cp "$apk" "$work/arm64.apk"
others=$(unzip -Z1 "$work/arm64.apk" | grep -E '^lib/(armeabi-v7a|x86|x86_64)/' || true)
if [ -n "$others" ]; then
  # shellcheck disable=SC2086
  zip -q -d "$work/arm64.apk" $others
fi
unzip -Z1 "$work/arm64.apk" | grep -q '^lib/arm64-v8a/' || { echo "android-sign: no arm64-v8a libraries" >&2; exit 1; }

if [ -z "${ANDROID_KEYSTORE_B64:-}" ]; then
  echo "::warning::no Android signing key: the APKs and AAB are unsigned"
  "$bt/zipalign" -P 16 -f 4 "$apk" dist/pubky-music-android-universal-unsigned.apk
  "$bt/zipalign" -P 16 -f 4 "$work/arm64.apk" dist/pubky-music-android-arm64-unsigned.apk
  cp "$aab" dist/pubky-music-android-unsigned.aab
  exit 0
fi

printf '%s' "$ANDROID_KEYSTORE_B64" | base64 -d > "$work/release.p12"
sign_apk() {
  "$bt/zipalign" -P 16 -f 4 "$1" "$work/aligned.apk"
  "$bt/apksigner" sign --ks "$work/release.p12" --ks-type PKCS12 \
    --ks-pass env:ANDROID_KEYSTORE_PASSWORD --key-pass env:ANDROID_KEY_PASSWORD \
    --ks-key-alias "$ANDROID_KEY_ALIAS" --v4-signing-enabled false --out "$2" "$work/aligned.apk"
  "$bt/apksigner" verify --print-certs "$2" | grep -m1 'SHA-256'
}
sign_apk "$apk" dist/pubky-music-android-universal.apk
sign_apk "$work/arm64.apk" dist/pubky-music-android-arm64.apk
cp "$aab" dist/pubky-music-android.aab
jarsigner -keystore "$work/release.p12" -storetype PKCS12 \
  -storepass:env ANDROID_KEYSTORE_PASSWORD -keypass:env ANDROID_KEY_PASSWORD \
  -sigalg SHA256withRSA -digestalg SHA-256 dist/pubky-music-android.aab "$ANDROID_KEY_ALIAS" >/dev/null
jarsigner -verify dist/pubky-music-android.aab | tail -1
ls -l dist
