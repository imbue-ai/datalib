#!/usr/bin/env bash
# Build a signed + notarized Datalib.app and .dmg for distribution.
#
# Three phases, each its own process, so that nothing a build runs ever
# has the signing material in reach. docs/dev/release_steps.md has why.
#
#   compile        `tauri build --no-bundle`: runs `beforeBuildCommand`
#                  and compiles the shell. Every build script and proc
#                  macro runs here.
#   sign           Reads the secrets, signs what compile staged, then
#                  `tauri bundle`: bundler, codesign, notarytool, dmg.
#                  `tauri bundle` never compiles, and this phase contacts
#                  no package registry.
#   verify <dmg>   Copies the app out of the dmg and runs it, which runs
#                  the JS trees it bundles. Needs no build and no secret.
#
#   ./build-signed-app.sh           all three, for a local run
#   ./build-signed-app.sh <phase>   one, as release.yml's macos-app jobs do,
#                                   each on its own machine
#
# compile and verify refuse to start with signing material around.
#
#   CI      exports the three signing secrets into `sign`'s environment
#           from Vault (imbue-ai/use-vault-secrets).
#   local   any secret missing from the environment is fetched with the
#           vault CLI. The secrets live under restricted/, which the
#           default employee role cannot read, so log in with the
#           all-secrets role first:
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
#   DATALIB_APP_VERSION  overrides tauri.conf.json's `version` (the tag
#                            workflow passes the tag's version so the bundle,
#                            dmg filename, and About dialog all report the
#                            released version instead of the checked-in one).
#                            Give all three phases the same one.
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
# notarization + stapling after signing (all exported by `sign`).
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
script="$here/build-signed-app.sh"

# A second layer behind the job split, which is the real boundary: this
# catches a step moved to the wrong job. ACTIONS_ID_TOKEN_REQUEST_TOKEN is
# what lets any process in a job with `id-token: write` ask for the token
# Vault trades the secrets for.
refuse_signing_material() {
    local phase="$1" name
    for name in CERTS_TAR_GZ APPLE_API_KEY_ID APPLE_API_ISSUER APPLE_API_KEY \
        APPLE_API_KEY_PATH APPLE_SIGNING_IDENTITY ACTIONS_ID_TOKEN_REQUEST_TOKEN; do
        if [[ -n "${!name:-}" ]]; then
            echo "ERROR: $name is set, and the $phase phase runs third-party code." >&2
            echo "       Run it where signing material is out of reach." >&2
            exit 1
        fi
    done
}

compile() {
    cd "$here"
    # pnpm on PATH via corepack, same shim the other dev scripts use.
    # shellcheck source=../../scripts/ensure_pnpm.sh
    source "$here/../../scripts/ensure_pnpm.sh"
    # The Tauri CLI at the version pnpm-lock.yaml names, from the tarballs
    # it names, with no install script run — never `pnpm dlx`.
    pnpm install --frozen-lockfile --ignore-scripts
    # The bundled binaries as the release tarballs build them: optimized,
    # and stamped so `datalib-dag --version` names the release. Set here
    # rather than defaulted, since this is what ships.
    export DATALIB_COMPILATION_MODE=opt DATALIB_STAMP=1
    if [[ -n "${DATALIB_APP_VERSION:-}" ]]; then
        pnpm exec tauri build --no-bundle --config "{\"version\":\"$DATALIB_APP_VERSION\"}" -- --locked
    else
        pnpm exec tauri build --no-bundle -- --locked
    fi
}

cleanup_signing() {
    # delete-keychain also drops the entry from the search list, so the
    # search list ends up exactly as it started.
    if [[ -n "$keychain_created" ]]; then
        security delete-keychain "$keychain" 2>/dev/null || true
    fi
    rm -rf "$workdir"
}

sign() {
    cd "$here"
    # Straight from node_modules: pnpm could reach for the registry.
    local tauri="$here/node_modules/.bin/tauri"
    if [[ ! -x "$tauri" ]]; then
        echo "ERROR: no Tauri CLI at $tauri; the compile phase installs it." >&2
        exit 1
    fi

    # Fetch whichever of the three secrets the environment doesn't already
    # provide (all three in a local run; none in CI).
    local vault_dir=restricted/datalib-release name value
    local missing=()
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
                cat >&2 <<'MSG'
ERROR: Vault read failed. These secrets live under restricted/, which the
default employee role cannot read. Log in with the all-secrets role first:

    vault login -method oidc role=employee_all_secrets
MSG
                exit 1
            fi
            export "$name=$value"
        done
        unset value
    fi

    keychain=datalib-codesign.keychain
    keychain_created=""
    workdir="$(mktemp -d)"
    trap cleanup_signing EXIT

    printf %s "$CERTS_TAR_GZ" | base64 -d | tar -xz -C "$workdir" -f -
    local certs="$workdir/certs" p12_password
    p12_password="$(tr -d '\n\r' < "$certs/p12_password.txt")"

    local notary_key="$certs/AuthKey_${APPLE_API_KEY_ID}.p8"
    if [[ ! -f "$notary_key" ]]; then
        echo "ERROR: $notary_key not in the certs tarball — APPLE_API_KEY_ID and CERTS_TAR_GZ disagree." >&2
        exit 1
    fi

    # A previous run killed hard enough to skip the trap leaves a stale
    # keychain behind; clear it so create-keychain can't fail.
    security delete-keychain "$keychain" 2>/dev/null || true

    local keychain_password
    keychain_password="$(uuidgen)"
    security create-keychain -p "$keychain_password" "$keychain"
    keychain_created=1
    # No -t: never auto-lock — the notarization wait can exceed any timeout.
    security set-keychain-settings "$keychain"
    security unlock-keychain -p "$keychain_password" "$keychain"

    local p12 cer
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
    local line
    local existing_keychains=()
    while IFS= read -r line; do
        line="${line//\"/}"
        line="${line#"${line%%[![:space:]]*}"}"
        [[ "$line" == *"$keychain"* ]] && continue
        [[ -n "$line" ]] && existing_keychains+=("$line")
    done < <(security list-keychains -d user)
    security list-keychains -d user -s "$keychain" "${existing_keychains[@]}"

    # Sign by SHA-1 hash rather than name so codesign can't pick a same-named
    # identity from another keychain.
    local identity
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

    "$here/sign-staged.sh"

    # shellcheck disable=SC2054  # app,dmg is one comma-separated CLI argument
    local bundle_args=(bundle --bundles app,dmg)
    if [[ -n "${DATALIB_APP_VERSION:-}" ]]; then
        bundle_args+=(--config "{\"version\":\"$DATALIB_APP_VERSION\"}")
    fi

    # The dmg maker (AppleScript + hdiutil) intermittently fails to detach the
    # volume, same flake sculptor hits; retry, clearing any stale mount between
    # tries.
    local attempt=0
    until "$tauri" "${bundle_args[@]}"; do
        attempt=$((attempt + 1))
        if (( attempt >= 3 )); then
            echo "ERROR: tauri bundle failed after $attempt attempts." >&2
            exit 1
        fi
        echo "tauri bundle failed; retry $attempt after clearing any stale dmg volume..." >&2
        hdiutil detach /Volumes/Datalib 2>/dev/null || true
        sleep 5
    done
}

# What a person gets: the app dragged out of the dmg, not the build's copy.
verify() {
    local dmg="$1"
    # Resolved: on a mac mktemp names /var/folders, a link to /private/var,
    # and check-app.sh compares this path with what the tools report.
    verify_dir="$(cd "$(mktemp -d)" && pwd -P)"
    trap 'hdiutil detach "$verify_dir/volume" 2>/dev/null || true; rm -rf "$verify_dir"' EXIT
    hdiutil attach -nobrowse -readonly -mountpoint "$verify_dir/volume" "$dmg" >/dev/null
    ditto "$verify_dir/volume/Datalib.app" "$verify_dir/Datalib.app"
    hdiutil detach "$verify_dir/volume" >/dev/null
    local app_bundle="$verify_dir/Datalib.app"

    codesign --verify --deep --strict "$app_bundle"
    xcrun stapler validate "$app_bundle"
    # The end-to-end Gatekeeper check: fails unless signed AND notarized.
    spctl --assess --type execute "$app_bundle"
    # Signed and notarized is not the same as runs: the bundled Node and
    # both JS trees have to start from inside the .app.
    "$here/check-app.sh" "$app_bundle"
    # macOS refuses to start a program whose signature cannot back its
    # entitlements (Datalib.entitlements), and says so only by killing it.
    "$app_bundle/Contents/MacOS/datalib-tauri" --version
    # Optimized, and stamped with the version this bundle claims: the tag's
    # in CI, `git describe` (what the stamp reads) in a local run.
    local stamped
    if [[ -n "${DATALIB_APP_VERSION:-}" ]]; then
        stamped="v$DATALIB_APP_VERSION"
    else
        stamped="$(git -C "$here" describe --tags --match 'v[0-9]*' --always --dirty)"
    fi
    "$here/../../scripts/release/check_binaries.sh" "$stamped" "$app_bundle/Contents/Resources/binaries"
    echo "Verified: $dmg"
}

phase="${1:-all}"

# Before the platform check, so //tools:build_signed_app_test can run this
# on Linux, which is where CI runs it.
case "$phase" in
    compile | verify | all) refuse_signing_material "$phase" ;;
    sign) ;;
    *)
        echo "usage: $0 [compile|sign|verify <dmg>]" >&2
        exit 2
        ;;
esac

if [[ "$(uname -s)" != Darwin ]]; then
    echo "ERROR: this script needs macOS (codesign / notarytool)." >&2
    exit 1
fi

case "$phase" in
    compile) compile ;;
    sign) sign ;;
    verify) verify "${2:?usage: $0 verify <dmg>}" ;;
    all)
        "$BASH" "$script" compile
        "$BASH" "$script" sign
        # The target dir may be redirected (e.g. a shared CARGO_TARGET_DIR or
        # a `build.target-dir` config so worktrees reuse one cache) — ask cargo
        # where it actually built instead of assuming ./target.
        target_dir="$(cd "$here" && cargo metadata --no-deps --format-version 1 \
            | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')"
        dmg="$(ls -t "$target_dir"/release/bundle/dmg/Datalib_*.dmg | head -n 1)"
        "$BASH" "$script" verify "$dmg"
        echo
        echo "Signed + notarized: $dmg"
        ;;
esac
