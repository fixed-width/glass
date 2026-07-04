#!/usr/bin/env bash
# Notarize and staple a signed GlassMcp.app: submit it to Apple's notary service,
# wait, staple the ticket into the bundle, and verify. Requires an App Store Connect
# API key (App Manager role). Notarization covers the nested clip-shim dylib too.
#
#   ./packaging/macos/notarize.sh --app <path.app> \
#       --key <AuthKey.p8> --key-id <KEYID> --issuer <ISSUER_UUID>
#
# Fails fast (non-zero, actionable) if not on macOS, the app is missing/unsigned, the
# signature lacks a secure timestamp (notarization would reject it), or a credential is
# absent. On a notarization failure it dumps `xcrun notarytool log` before exiting.
set -euo pipefail

app="" key="" key_id="" issuer=""
while [ $# -gt 0 ]; do
  case "$1" in
    --app)    app="$2"; shift 2 ;;
    --key)    key="$2"; shift 2 ;;
    --key-id) key_id="$2"; shift 2 ;;
    --issuer) issuer="$2"; shift 2 ;;
    -h|--help)
      echo "usage: notarize.sh --app PATH --key AuthKey.p8 --key-id ID --issuer UUID" >&2
      exit 1 ;;
    *) echo "error: unknown argument: $1" >&2; exit 1 ;;
  esac
done

[ "$(uname -s)" = "Darwin" ] || { echo "error: notarize.sh must run on macOS" >&2; exit 1; }
for v in app key key_id issuer; do
  [ -n "${!v}" ] || { echo "error: --${v//_/-} is required" >&2; exit 1; }
done
[ -d "$app" ]  || { echo "error: app bundle not found: $app" >&2; exit 1; }
[ -f "$key" ]  || { echo "error: API key file not found: $key" >&2; exit 1; }

# Notarization requires a valid signature WITH a secure timestamp; catch a missing one
# here rather than after a slow submit round-trip.
codesign --verify --strict --deep -vv "$app" \
  || { echo "error: $app is not validly signed (sign it before notarizing)" >&2; exit 1; }
codesign -dvv "$app" 2>&1 | grep -qi "Timestamp=" \
  || { echo "error: $app signature has no secure timestamp — re-sign with build-app.sh --timestamp" >&2; exit 1; }

# notarytool needs a container to upload; we staple the .app itself afterward.
sub="$(dirname "$app")/notarize-submission.zip"
rm -f "$sub"
ditto -c -k --keepParent "$app" "$sub"

echo "==> submitting $(basename "$app") to Apple notary service (this can take minutes)"
if ! out="$(xcrun notarytool submit "$sub" \
      --key "$key" --key-id "$key_id" --issuer "$issuer" --wait 2>&1)"; then
  echo "$out"
  sub_id="$(printf '%s\n' "$out" | awk '/id:/ {print $2; exit}')"
  if [ -n "${sub_id:-}" ]; then
    echo "==> notarization failed; fetching log for $sub_id"
    xcrun notarytool log "$sub_id" --key "$key" --key-id "$key_id" --issuer "$issuer" || true
  fi
  rm -f "$sub"
  exit 1
fi
printf '%s\n' "$out"
rm -f "$sub"

echo "==> stapling ticket into $(basename "$app")"
xcrun stapler staple "$app"

echo "==> verifying notarized bundle"
xcrun stapler validate "$app"
spctl -a -vvv -t exec "$app"
codesign --verify --deep --strict -vv "$app"
echo "==> notarized + stapled: $app"
