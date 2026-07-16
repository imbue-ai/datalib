#!/usr/bin/env bash
# Build a signed + notarized Frankweiler.app and .dmg for distribution.
#
# One script serves both local runs and CI (the macos-app job in
# .github/workflows/release.yml) so the two paths can't drift:
#
#   CI      exports the three signing secrets into the environment from
#           Vault (imbue-ai/use-vault-secrets) and then runs this script.
#   local   just run ./build-signed-app.sh — any secret missing from the
#           environment is fetched with the vault CLI. The secrets live
#           under restricted/, which the default employee role cannot
#           read, so log in with the all-secrets role first:
#               vault login -method oidc role=employee_all_secrets
#
# Secrets (Vault: restricted/datalib-release/*):
#   CERTS_TAR_GZ      base64 tar.gz holding the Developer ID .p12 pair +
#                     p12_password.txt + Apple intermediate CA .cer files +
#                     the notarization key AuthKey_<APPLE_API_KEY_ID>.p8
#   APPLE_API_KEY_ID  App Store Connect API key id (notarization)
#   APPLE_API_ISSUER  App Store Connect issuer id (notarization)
#
# Optional:
#   FRANKWEILER_APP_VERSION  overrides tauri.conf.json's `version` (the tag
#                            workflow passes the tag's version so the bundle,
#                            dmg filename, and About dialog all report the
#                            released version instead of the checked-in one).
#
# The signing identity is imported into a throwaway keychain that is
# deleted on exit, so local runs never touch the login keychain and no
# sudo is needed (unlike the sculptor-era import_cert.sh shipped inside
# the certs tarball, which changes the default keychain and writes to the
# System keychain). The Apple intermediate CAs are imported next to the
# identity because codesign needs them to build the signature chain and
# bare macOS runners don't always have them.
#
# Tauri picks everything up from the environment: APPLE_SIGNING_IDENTITY
# selects the codesign identity, and the APPLE_API_* variables switch on
# notarization + stapling after signing (both exported below).
set -euo pipefail

if [[ "$(uname -s)" != Darwin ]]; then
    echo "ERROR: this script needs macOS (codesign / notarytool)." >&2
    exit 1
fi

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$here"

# pnpm on PATH via corepack, same shim the other dev scripts use.
# shellcheck source=../../scripts/ensure_pnpm.sh
source "$here/../../scripts/ensure_pnpm.sh"

vault_dir=restricted/datalib-release

# ---- Signing secrets ---------------------------------------------------
# Fetch whichever of the three secrets the environment doesn't already
# provide (all three in a local run; none in CI).
missing=()
for name in CERTS_TAR_GZ APPLE_API_KEY_ID APPLE_API_ISSUER; do
    [[ -n "${!name:-}" ]] || missing+=("$name")
done
if (( ${#missing[@]} > 0 )); then
    export VAULT_ADDR="https://vault-cluster-public-vault-df29b16f.9b573ab7.z1.hashicorp.cloud:8200"
    export VAULT_NAMESPACE=admin
    if ! command -v vault >/dev/null 2>&1; then
        echo "ERROR: ${missing[*]} not set and no \`vault\` CLI to fetch them with." >&2
        echo "       brew install hashicorp/tap/vault" >&2
        exit 1
    fi
    echo "Fetching from Vault: ${missing[*]}" >&2
    for name in "${missing[@]}"; do
        if ! value="$(vault kv get -field=value -mount=secrets "$vault_dir/$name")"; then
            cat >&2 <<'EOF'
ERROR: Vault read failed. These secrets live under restricted/, which the
default employee role cannot read. Log in with the all-secrets role first:

    vault login -method oidc role=employee_all_secrets
EOF
            exit 1
        fi
        export "$name=$value"
    done
    unset value
fi

# ---- Throwaway signing keychain ----------------------------------------
keychain=frankweiler-codesign.keychain
keychain_created=""
workdir="$(mktemp -d)"

cleanup() {
    # delete-keychain also drops the entry from the search list, so the
    # search list ends up exactly as it started.
    if [[ -n "$keychain_created" ]]; then
        security delete-keychain "$keychain" 2>/dev/null || true
    fi
    rm -rf "$workdir"
}
trap cleanup EXIT

printf %s "$CERTS_TAR_GZ" | base64 -d | tar -xz -C "$workdir" -f -
certs="$workdir/certs"
p12_password="$(tr -d '\n\r' < "$certs/p12_password.txt")"

notary_key="$certs/AuthKey_${APPLE_API_KEY_ID}.p8"
if [[ ! -f "$notary_key" ]]; then
    echo "ERROR: $notary_key not in the certs tarball — APPLE_API_KEY_ID and CERTS_TAR_GZ disagree." >&2
    exit 1
fi

# A previous run killed hard enough to skip the trap leaves a stale
# keychain behind; clear it so create-keychain can't fail.
security delete-keychain "$keychain" 2>/dev/null || true

keychain_password="$(uuidgen)"
security create-keychain -p "$keychain_password" "$keychain"
keychain_created=1
# No -t: never auto-lock — the notarization wait can exceed any timeout.
security set-keychain-settings "$keychain"
security unlock-keychain -p "$keychain_password" "$keychain"

for p12 in "$certs"/*.p12; do
    security import "$p12" -k "$keychain" -P "$p12_password" -T /usr/bin/codesign >/dev/null
done
for cer in "$certs"/*.cer; do
    security import "$cer" -k "$keychain" >/dev/null
done
# Let codesign use the imported keys without a UI confirmation prompt
# (required since macOS Sierra; the output dumps key attributes, silence it).
security set-key-partition-list -S apple-tool:,apple:,codesign: \
    -s -k "$keychain_password" "$keychain" >/dev/null

# codesign resolves identities through the keychain search list, so the
# throwaway keychain must be on it. Rebuild the list as ours + the current
# entries (minus any stale copy of ours). delete-keychain undoes this.
existing_keychains=()
while IFS= read -r line; do
    line="${line//\"/}"
    line="${line#"${line%%[![:space:]]*}"}"
    [[ "$line" == *"$keychain"* ]] && continue
    [[ -n "$line" ]] && existing_keychains+=("$line")
done < <(security list-keychains -d user)
security list-keychains -d user -s "$keychain" "${existing_keychains[@]}"

# Sign by SHA-1 hash rather than name so codesign can't pick a same-named
# identity from another keychain.
identity="$(security find-identity -v -p codesigning "$keychain" \
    | awk '/Developer ID Application/ {print $2; exit}')"
if [[ -z "$identity" ]]; then
    echo "ERROR: no 'Developer ID Application' identity found in the imported certs:" >&2
    security find-identity -v -p codesigning "$keychain" >&2
    exit 1
fi
echo "Signing identity: $(security find-identity -v -p codesigning "$keychain" | awk -F'"' '/Developer ID Application/ {print $2; exit}')"

export APPLE_SIGNING_IDENTITY="$identity"
export APPLE_API_KEY="$APPLE_API_KEY_ID"
export APPLE_API_KEY_PATH="$notary_key"
export APPLE_API_ISSUER

# ---- Build, sign, notarize ----------------------------------------------
# shellcheck disable=SC2054  # app,dmg is one comma-separated CLI argument
build_args=(build --bundles app,dmg)
if [[ -n "${FRANKWEILER_APP_VERSION:-}" ]]; then
    build_args+=(--config "{\"version\":\"$FRANKWEILER_APP_VERSION\"}")
fi

# The dmg maker (AppleScript + hdiutil) intermittently fails to detach the
# volume, same flake sculptor hits; retry, clearing any stale mount between
# tries. A retry after a real build failure is cheap: everything up to the
# failure is incremental.
attempt=0
until pnpm dlx @tauri-apps/cli@2 "${build_args[@]}"; do
    attempt=$((attempt + 1))
    if (( attempt >= 3 )); then
        echo "ERROR: tauri build failed after $attempt attempts." >&2
        exit 1
    fi
    echo "tauri build failed; retry $attempt after clearing any stale dmg volume..." >&2
    hdiutil detach /Volumes/Frankweiler 2>/dev/null || true
    sleep 5
done

# ---- Verify what we're about to ship -------------------------------------
# The target dir may be redirected (e.g. a shared CARGO_TARGET_DIR or a
# `build.target-dir` config so worktrees reuse one cache) — ask cargo
# where it actually built instead of assuming ./target.
target_dir="$(cargo metadata --no-deps --format-version 1 \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')"
app_bundle="$target_dir/release/bundle/macos/Frankweiler.app"
dmg="$(ls -t "$target_dir"/release/bundle/dmg/Frankweiler_*.dmg | head -n 1)"

codesign --verify --deep --strict "$app_bundle"
xcrun stapler validate "$app_bundle"
# The end-to-end Gatekeeper check: fails unless signed AND notarized.
spctl --assess --type execute "$app_bundle"

echo
echo "Signed + notarized:"
echo "  app: $app_bundle"
echo "  dmg: $dmg"
