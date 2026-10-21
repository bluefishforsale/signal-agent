#!/usr/bin/env bash
# Release gate for PRD acceptance 11: a tag build with any signing input missing
# fails here, before anything is built, signed or published. It checks presence
# only and never prints a value.
set -euo pipefail

# NOTE: names come from docs/signing-setup.md. release.yml must map each one into
# this step's env; a name it forgets to map reads as missing, which fails closed.
required=(
  APPLE_CERTIFICATE
  APPLE_CERTIFICATE_PASSWORD
  APPLE_SIGNING_IDENTITY
  APPLE_TEAM_ID
  APPLE_API_ISSUER
  APPLE_API_KEY
  APPLE_API_PRIVATE_KEY
  KEYCHAIN_PASSWORD
  AZURE_TENANT_ID
  AZURE_CLIENT_ID
  AZURE_SUBSCRIPTION_ID
  AZURE_SIGNING_ENDPOINT
  AZURE_SIGNING_ACCOUNT
  AZURE_CERT_PROFILE
  TAURI_SIGNING_PRIVATE_KEY
  TAURI_SIGNING_PRIVATE_KEY_PASSWORD
)

missing=()
for name in "${required[@]}"; do
  [[ -n "${!name:-}" ]] || missing+=("$name")
done

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
