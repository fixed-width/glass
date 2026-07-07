#!/usr/bin/env bash
# Notarize and staple a signed GlassMcp.app, OR codesign+notarize+staple a .dmg produced by
# make-dmg.sh: submit the container to Apple's notary service, wait, staple the ticket into
# it, and verify. Requires an App Store Connect API key (App Manager role). Notarization
# covers the nested clip-shim dylib too.
#
#   ./packaging/macos/notarize.sh --app <path.app> \
#       --key <AuthKey.p8> --key-id <KEYID> --issuer <ISSUER_UUID>
#
#   ./packaging/macos/notarize.sh --dmg <path.dmg> \
#       --identity <signing identity> [--keychain <path>] \
#       --key <AuthKey.p8> --key-id <KEYID> --issuer <ISSUER_UUID>
#
# --app and --dmg are mutually exclusive; exactly one is required.
#
# --app mode: the .app is assumed already signed (by build-app.sh) — this verifies the
# signature/timestamp, zips the bundle, submits the zip, staples the .app, and verifies with
# `stapler validate` + `spctl -a -t exec`.
#
# --dmg mode: make-dmg.sh produces an UNSIGNED image, and notarytool rejects unsigned
# submissions — so this mode codesigns the .dmg itself first (a .dmg is a container, not an
# executable, so NO `--options runtime`), submits the .dmg directly, staples it, and verifies
# with `spctl -a -t open` (a .dmg is checked with `-t open`, not `-t exec`; `stapler validate`
# is not used on a .dmg).
#
#   --identity NAME   codesign identity (a Keychain "Common Name") to sign the .dmg with.
#                     Required in --dmg mode, unused in --app mode. (env: GLASS_SIGN_IDENTITY)
#   --keychain PATH   keychain to resolve --identity from, passed straight to
#                     `codesign --keychain` (default: the keychain search list).
#                     Optional, --dmg mode only. (env: GLASS_SIGN_KEYCHAIN)
#
# Fails fast (non-zero, actionable) if not on macOS, the container is missing/unsigned, the
# signature lacks a secure timestamp (notarization would reject it), or a credential is
# absent. On a notarization failure it dumps `xcrun notarytool log` before exiting.
set -euo pipefail

app="" dmg="" key="" key_id="" issuer=""
sign_identity="${GLASS_SIGN_IDENTITY:-}"
sign_keychain="${GLASS_SIGN_KEYCHAIN:-}"
while [ $# -gt 0 ]; do
  case "$1" in
    --app)      [ $# -ge 2 ] || { echo "error: --app requires a value" >&2; exit 1; }; app="$2"; shift 2 ;;
    --dmg)      [ $# -ge 2 ] || { echo "error: --dmg requires a value" >&2; exit 1; }; dmg="$2"; shift 2 ;;
    --identity) [ $# -ge 2 ] || { echo "error: --identity requires a value" >&2; exit 1; }; sign_identity="$2"; shift 2 ;;
    --keychain) [ $# -ge 2 ] || { echo "error: --keychain requires a value" >&2; exit 1; }; sign_keychain="$2"; shift 2 ;;
    --key)      [ $# -ge 2 ] || { echo "error: --key requires a value" >&2; exit 1; }; key="$2"; shift 2 ;;
    --key-id)   [ $# -ge 2 ] || { echo "error: --key-id requires a value" >&2; exit 1; }; key_id="$2"; shift 2 ;;
    --issuer)   [ $# -ge 2 ] || { echo "error: --issuer requires a value" >&2; exit 1; }; issuer="$2"; shift 2 ;;
    -h|--help)
      echo "usage: notarize.sh --app PATH --key AuthKey.p8 --key-id ID --issuer UUID" >&2
      echo "       notarize.sh --dmg PATH --identity NAME [--keychain PATH] --key AuthKey.p8 --key-id ID --issuer UUID" >&2
      exit 1 ;;
    *) echo "error: unknown argument: $1" >&2; exit 1 ;;
  esac
done

[ "$(uname -s)" = "Darwin" ] || { echo "error: notarize.sh must run on macOS" >&2; exit 1; }

if [ -n "$app" ] && [ -n "$dmg" ]; then
  echo "error: --app and --dmg are mutually exclusive" >&2
  exit 1
fi
if [ -z "$app" ] && [ -z "$dmg" ]; then
  echo "error: one of --app or --dmg is required" >&2
  exit 1
fi

for v in key key_id issuer; do
  [ -n "${!v}" ] || { echo "error: --${v//_/-} is required" >&2; exit 1; }
done
[ -f "$key" ] || { echo "error: API key file not found: $key" >&2; exit 1; }

# Shared by both modes: submit a container to Apple's notary service and wait, print its
# output, and on failure extract the submission id and dump `xcrun notarytool log` before
# exiting non-zero.
submit_and_wait() { # <container path> <label for progress echo>
  local container="$1" label="$2"
  echo "==> submitting $label to Apple notary service (this can take minutes)"
  local out
  if ! out="$(xcrun notarytool submit "$container" \
        --key "$key" --key-id "$key_id" --issuer "$issuer" --wait 2>&1)"; then
    echo "$out"
    local sub_id
    sub_id="$(printf '%s\n' "$out" | awk '/id:/ {print $2; exit}')"
    if [ -n "${sub_id:-}" ]; then
      echo "==> notarization failed; fetching log for $sub_id"
      xcrun notarytool log "$sub_id" --key "$key" --key-id "$key_id" --issuer "$issuer" || true
    fi
    exit 1
  fi
  printf '%s\n' "$out"
}

if [ -n "$app" ]; then
  [ -d "$app" ] || { echo "error: app bundle not found: $app" >&2; exit 1; }

  # Notarization requires a valid signature WITH a secure timestamp; catch a missing one
  # here rather than after a slow submit round-trip.
  codesign --verify --strict --deep -vv "$app" \
    || { echo "error: $app is not validly signed (sign it before notarizing)" >&2; exit 1; }

  # Notarization requires a secure timestamp on the app AND its nested signed code;
  # catch a missing one here rather than after a slow submit round-trip.
  require_timestamp() { # <signed path>
    codesign -dvv "$1" 2>&1 | grep -qi "Timestamp=" \
      || { echo "error: $1 signature has no secure timestamp — re-sign with build-app.sh --timestamp" >&2; exit 1; }
  }
  require_timestamp "$app"
  if [ -d "$app/Contents/Frameworks" ]; then
    for dylib in "$app/Contents/Frameworks"/*.dylib; do
      [ -e "$dylib" ] || continue   # no match → skip the literal glob
      require_timestamp "$dylib"
    done
  fi

  # notarytool needs a container to upload; we staple the .app itself afterward.
  sub="$(dirname "$app")/notarize-submission.zip"
  trap 'rm -f "$sub"' EXIT
  rm -f "$sub"
  ditto -c -k --keepParent "$app" "$sub"

  submit_and_wait "$sub" "$(basename "$app")"

  echo "==> stapling ticket into $(basename "$app")"
  xcrun stapler staple "$app"

  echo "==> verifying notarized bundle"
  xcrun stapler validate "$app"
  spctl -a -vvv -t exec "$app"
  codesign --verify --deep --strict -vv "$app"
  echo "==> notarized + stapled: $app"
else
  [ -f "$dmg" ] || { echo "error: dmg not found: $dmg" >&2; exit 1; }
  [ -n "$sign_identity" ] \
    || { echo "error: --identity (or GLASS_SIGN_IDENTITY) is required in --dmg mode" >&2; exit 1; }

  # make-dmg.sh produces an unsigned image, and notarytool rejects unsigned submissions, so
  # sign the .dmg here first. A .dmg is a container, not an executable — no `--options
  # runtime` (that's for hardened-runtime executables only).
  echo "==> codesigning $(basename "$dmg") (identity: $sign_identity)"
  codesign_args=(--timestamp -s "$sign_identity")
  if [ -n "$sign_keychain" ]; then
    codesign_args+=(--keychain "$sign_keychain")
  fi
  codesign "${codesign_args[@]}" "$dmg"

  submit_and_wait "$dmg" "$(basename "$dmg")"

  echo "==> stapling ticket into $(basename "$dmg")"
  xcrun stapler staple "$dmg"

  echo "==> verifying notarized dmg"
  spctl -a -t open --context context:primary-signature "$dmg"
  echo "==> notarized + stapled: $dmg"
fi
