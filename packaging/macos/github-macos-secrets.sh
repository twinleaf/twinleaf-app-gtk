#!/usr/bin/env bash
# Prepare the macOS signing/notarization secrets the Release workflow needs and
# push them to the GitHub repo with `gh`. Run this yourself — secrets go from
# your machine straight to GitHub over HTTPS; nothing is printed.
#
# One-time prerequisite (the private key): export your Developer ID identity
# from Keychain Access as a .p12 — `security` cannot export a single identity
# non-interactively, and a bulk export would dump every private key you own.
#
#   1. Open Keychain Access ▸ "login" keychain ▸ My Certificates.
#   2. Select  "Developer ID Application: Twinleaf LLC (6MFC6VKTVX)"  (expand it
#      and confirm a private key is nested underneath).
#   3. Right-click ▸ Export… ▸ Personal Information Exchange (.p12) ▸ pick a
#      strong password ▸ save e.g. to ~/Desktop/DeveloperID.p12.
#
# Then:
#   export APPLE_ID="kornack@twinleaf.com"
#   export APPLE_APP_PASSWORD="xxxx-xxxx-xxxx-xxxx"   # app-specific password
#   export APPLE_TEAM="6MFC6VKTVX"                    # optional; read from cert
#   packaging/macos/github-macos-secrets.sh --p12 ~/Desktop/DeveloperID.p12
#
# Add --print to write the values to ./secrets-out/ + emit the `gh` commands
# instead of pushing (for when `gh` isn't installed/authenticated).
set -euo pipefail
cd "$(cd "$(dirname "$0")/../.." && pwd)"

P12=""; MODE="push"
while [ $# -gt 0 ]; do
  case "$1" in
    --p12) P12="$2"; shift 2 ;;
    --print) MODE="print"; shift ;;
    --repo) GITHUB_REPO="$2"; shift 2 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

# --- resolve target repo (the remote whose URL is on github.com) -------------
REPO="${GITHUB_REPO:-$(git remote -v | awk '/github\.com/ {print $2; exit}' \
  | sed -E 's#.*github\.com[:/]##; s#\.git$##')}"
[ -n "$REPO" ] || { echo "error: no github.com remote found; pass --repo owner/name" >&2; exit 1; }

# --- signing identity + team (prefer the cert matching APPLE_TEAM) -----------
TEAM="${APPLE_TEAM:-${APPLE_TEAM_ID:-}}"
list="$(security find-identity -v -p codesigning)"
if [ -n "$TEAM" ]; then
  SIGN_IDENTITY="$(printf '%s\n' "$list" | awk -F'"' -v t="($TEAM)" \
    '/Developer ID Application/ && index($2, t) {print $2; exit}')"
fi
SIGN_IDENTITY="${SIGN_IDENTITY:-$(printf '%s\n' "$list" | awk -F'"' \
  '/Developer ID Application/ {print $2; exit}')}"
[ -n "$SIGN_IDENTITY" ] || { echo "error: no 'Developer ID Application' identity in keychain" >&2; exit 1; }
# Derive the team from the identity string if it wasn't supplied.
TEAM="${TEAM:-$(printf '%s' "$SIGN_IDENTITY" | sed -E 's/.*\(([A-Z0-9]{10})\)$/\1/')}"
echo "Repo:     $REPO"
echo "Identity: $SIGN_IDENTITY"
echo "Team:     $TEAM"

# --- the .p12 (exported from Keychain Access) --------------------------------
if [ -z "$P12" ]; then
  echo >&2
  echo "Next: export the single identity above from Keychain Access as a .p12," >&2
  echo "then re-run with --p12 /path/to/that.p12  (see the header of this script)." >&2
  command -v open >/dev/null && open -a "Keychain Access" || true
  exit 0
fi
[ -f "$P12" ] || { echo "error: no such file: $P12" >&2; exit 1; }

printf 'Password for %s: ' "$(basename "$P12")" >&2
read -rs P12_PASS; echo >&2
[ -n "$P12_PASS" ] || { echo "error: empty .p12 password" >&2; exit 1; }

# --- collect the Apple notarization values (from env, or prompt) -------------
APPLE_ID="${APPLE_ID:-}"
if [ -z "$APPLE_ID" ]; then printf 'APPLE_ID (Apple ID email): ' >&2; read -r APPLE_ID; fi
APPLE_APP_PASSWORD="${APPLE_APP_PASSWORD:-}"
if [ -z "$APPLE_APP_PASSWORD" ]; then
  printf 'APPLE_APP_PASSWORD (app-specific password): ' >&2; read -rs APPLE_APP_PASSWORD; echo >&2
fi
[ -n "$APPLE_ID" ] && [ -n "$APPLE_APP_PASSWORD" ] || { echo "error: APPLE_ID / APPLE_APP_PASSWORD required" >&2; exit 1; }

# --- scratch space, always cleaned up ----------------------------------------
WORK="$(mktemp -d)"
VERIFY_KC="$WORK/verify.keychain-db"
cleanup() {
  security delete-keychain "$VERIFY_KC" >/dev/null 2>&1 || true
  rm -rf "$WORK"
}
trap cleanup EXIT

# Validate the .p12 the same way CI will: import it, then confirm it yields a
# real Developer ID Application *identity* (cert + private key), not just one or
# the other — a key-only or cert-only .p12 imports fine but can't sign.
security create-keychain -p verify "$VERIFY_KC" >/dev/null
security unlock-keychain -p verify "$VERIFY_KC" >/dev/null 2>&1 || true
if ! security import "$P12" -k "$VERIFY_KC" -P "$P12_PASS" -T /usr/bin/codesign >/dev/null 2>&1; then
  echo "error: could not open the .p12 with that password." >&2
  exit 1
fi
if ! security find-identity -p codesigning "$VERIFY_KC" | grep -q "Developer ID Application"; then
  echo "error: the .p12 has no 'Developer ID Application' identity (cert + key)." >&2
  echo "Re-export from Keychain Access: My Certificates → right-click the" >&2
  echo "'Developer ID Application' row (private key nested under it) → Export → .p12." >&2
  exit 1
fi
echo "Verified: .p12 contains a Developer ID Application identity (cert + key)."

# A throwaway password for the ephemeral keychain CI builds on each run.
KEYCHAIN_PASSWORD="$(openssl rand -hex 24)"

# base64 of the .p12 (the workflow does `base64 --decode`).
base64 < "$P12" > "$WORK/p12.b64"

# --- emit -------------------------------------------------------------------
set_secret() { # name  <stdin: value>
  if [ "$MODE" = push ]; then
    gh secret set "$1" -R "$REPO"
  else
    umask 077; cat > "secrets-out/$1"
    echo "gh secret set $1 -R $REPO < secrets-out/$1"
  fi
}

if [ "$MODE" = push ]; then
  command -v gh >/dev/null || { echo "error: 'gh' not found; install it or re-run with --print" >&2; exit 1; }
  gh auth status >/dev/null 2>&1 || { echo "error: run 'gh auth login' first (need a token with repo scope)" >&2; exit 1; }
  printf 'Set 7 secrets on %s? [y/N] ' "$REPO" >&2; read -r ok
  [ "$ok" = y ] || [ "$ok" = Y ] || { echo "aborted." >&2; exit 1; }
else
  mkdir -p secrets-out
fi

set_secret MACOS_CERTIFICATE_P12       < "$WORK/p12.b64"
printf '%s' "$P12_PASS"          | set_secret MACOS_CERTIFICATE_PASSWORD
printf '%s' "$SIGN_IDENTITY"     | set_secret MACOS_SIGN_IDENTITY
printf '%s' "$KEYCHAIN_PASSWORD" | set_secret MACOS_KEYCHAIN_PASSWORD
printf '%s' "$APPLE_ID"          | set_secret APPLE_ID
printf '%s' "$TEAM"              | set_secret APPLE_TEAM_ID
printf '%s' "$APPLE_APP_PASSWORD" | set_secret APPLE_APP_PASSWORD

if [ "$MODE" = push ]; then
  echo "Done — 7 secrets set on $REPO. You can delete the .p12 now."
else
  echo
  echo "Wrote ./secrets-out/ (chmod 600). Run the gh commands above, then:"
  echo "  rm -rf secrets-out   # contains your private key — delete when done"
fi
