#!/usr/bin/env bash
# Release gate for PRD acceptance 11: a tag build with any signing input missing
# fails here, before anything is built, signed or published. It checks presence
# only and never prints a value.
set -euo pipefail

# Scope `all` (the default) gates a release; `no-windows` gates one shipped
# with WINDOWS_UNSIGNED=true; `apple` gates the macOS-only signing test, which
# needs no Windows or updater signing.
scope="${1:-all}"

# NOTE: names come from docs/signing-setup.md. release.yml must map each one into
# this step's env; a name it forgets to map reads as missing, which fails closed.
apple=(
  APPLE_CERTIFICATE
  APPLE_CERTIFICATE_PASSWORD
  APPLE_SIGNING_IDENTITY
  APPLE_TEAM_ID
  APPLE_API_ISSUER
  APPLE_API_KEY
  APPLE_API_PRIVATE_KEY
  KEYCHAIN_PASSWORD
)
azure=(
  AZURE_TENANT_ID
  AZURE_CLIENT_ID
  AZURE_SUBSCRIPTION_ID
  AZURE_SIGNING_ENDPOINT
  AZURE_SIGNING_ACCOUNT
  AZURE_CERT_PROFILE
)
updater=(
  TAURI_SIGNING_PRIVATE_KEY
  TAURI_SIGNING_PRIVATE_KEY_PASSWORD
)
case "$scope" in
  all) required=("${apple[@]}" "${azure[@]}" "${updater[@]}") ;;
  no-windows) required=("${apple[@]}" "${updater[@]}") ;;
  apple) required=("${apple[@]}") ;;
  *) echo "unknown scope \"$scope\" (want all, no-windows or apple)" >&2; exit 2 ;;
esac

missing=()
for name in "${required[@]}"; do
  [[ -n "${!name:-}" ]] || missing+=("$name")
done

# The updater's public key is committed rather than secret, but until it is
# the real one no release can be installed as an update.
config="$(dirname "$0")/../app/src-tauri/tauri.conf.json"
if [[ "$scope" != apple ]] \
  && ! jq -r .plugins.updater.pubkey "$config" | base64 -d 2>/dev/null | grep -q '^untrusted comment: minisign public key'; then
  missing+=("plugins.updater.pubkey in app/src-tauri/tauri.conf.json")
fi

if ((${#missing[@]} == 0)); then
  echo "All ${#required[@]} signing inputs are present in the release environment."
  exit 0
fi

{
  echo "### Release refused: ${#missing[@]} of ${#required[@]} signing inputs missing"
  echo
  printf -- "- \`%s\`\n" "${missing[@]}"
  echo
  echo "Add them to the \`release\` environment as described in docs/signing-setup.md. Nothing was built or published."
} >>"${GITHUB_STEP_SUMMARY:-/dev/stderr}"

printf 'missing: %s\n' "${missing[@]}"
echo "::error title=Release refused::${#missing[@]} signing input(s) missing from the release environment: ${missing[*]}"
exit 1
