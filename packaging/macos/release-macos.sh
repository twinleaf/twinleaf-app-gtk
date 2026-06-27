#!/usr/bin/env bash
# Build, sign, and notarize Twinleaf.app for distribution — on a Mac that
# already has a "Developer ID Application" certificate in its login keychain.
#
# Required env (notarization auth):
#   APPLE_ID            your Apple ID email
#   APPLE_TEAM          10-char Team ID  (also accepts APPLE_TEAM_ID)
#   APPLE_APP_PASSWORD  app-specific password from appleid.apple.com
#
# The signing certificate is read from your keychain automatically (the one
# whose Team ID matches APPLE_TEAM). Override with:
#   MACOS_SIGN_IDENTITY="Developer ID Application: … (TEAMID)"
#
# Apple ID + app password authenticate the *upload*; the certificate does the
# *signing*. Notarization rejects anything not signed with a Developer ID, so
# both are needed — but the cert is a one-time keychain install, not a secret
# you pass each build.
set -euo pipefail
cd "$(cd "$(dirname "$0")/../.." && pwd)"

APP="build/Twinleaf.app"
ARCH="$(uname -m)"                       # arm64 (Apple Silicon) or x86_64 (Intel)
ZIP="twinleaf-gtk-macos-${ARCH}.zip"
TEAM="${APPLE_TEAM:-${APPLE_TEAM_ID:-}}"

: "${APPLE_ID:?set APPLE_ID}"
: "${APPLE_APP_PASSWORD:?set APPLE_APP_PASSWORD}"
: "${TEAM:?set APPLE_TEAM (10-char Team ID)}"

# Pick the Developer ID Application identity (prefer the one for this Team).
if [ -n "${MACOS_SIGN_IDENTITY:-}" ]; then
  IDENTITY="$MACOS_SIGN_IDENTITY"
else
  IDENTITY="$(security find-identity -v -p codesigning \
    | awk -F'"' -v team="($TEAM)" '/Developer ID Application/ && index($2, team) {print $2; exit}')"
  [ -n "$IDENTITY" ] || IDENTITY="$(security find-identity -v -p codesigning \
    | awk -F'"' '/Developer ID Application/ {print $2; exit}')"
fi
if [ -z "$IDENTITY" ]; then
  echo "error: no 'Developer ID Application' certificate in your keychain." >&2
  echo "Install one: Xcode > Settings > Accounts > Manage Certificates > + ." >&2
  exit 1
fi
echo "==> Signing identity: $IDENTITY"

# Build + bundle + sign in one pass (bundle-macos.sh signs the nested dylibs,
# the GdkPixbuf/GIO modules, the binary, and the .app with the hardened runtime
# + entitlements whenever CODESIGN_IDENTITY is set).
echo "==> Building and bundling Twinleaf.app ($ARCH)…"
CODESIGN_IDENTITY="$IDENTITY" packaging/macos/bundle-macos.sh
codesign --verify --strict --verbose=2 "$APP"

echo "==> Zipping for notarization…"
ditto -c -k --keepParent "$APP" "$ZIP"

echo "==> Submitting to Apple notary service (this can take a few minutes)…"
xcrun notarytool submit "$ZIP" \
  --apple-id "$APPLE_ID" --team-id "$TEAM" --password "$APPLE_APP_PASSWORD" --wait

echo "==> Stapling the notarization ticket…"
xcrun stapler staple "$APP"
rm -f "$ZIP"
ditto -c -k --keepParent "$APP" "$ZIP"
xcrun stapler validate "$APP"

echo "==> Done: $ZIP"
echo "    signed by '$IDENTITY', notarized, and stapled — runs without Gatekeeper prompts."
