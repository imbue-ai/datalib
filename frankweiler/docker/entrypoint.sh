#!/bin/sh
# frankweiler container entrypoint.
#
# Bootstraps a per-`.latchkey` encryption key before exec'ing the user's
# command. Inside this image there is no Secret Service (no GNOME
# Keyring / KWallet) for `@napi-rs/keyring` to talk to, so latchkey's
# only option is its env-var fallback (`LATCHKEY_ENCRYPTION_KEY`).
# Asking every user to invent and pass a key would be a paper cut they'd
# get wrong half the time — losing the key means losing the credentials
# it encrypted — so we generate one once, persist it inside the
# bind-mounted `/root/.latchkey/` dir, and re-read it on every container
# start.
#
# Security model: the key lives in the same directory as the encrypted
# blobs it protects. This is roughly equivalent to running a desktop
# Linux box with an auto-unlocked keyring — it protects against
# accidental disclosure of `credentials.json.enc` alone, but offers
# nothing against an attacker who can read both files. See
# docs/dev/docker.md for the full caveat.
#
# Behavior:
#   * If `LATCHKEY_ENCRYPTION_KEY` is already set in the env, do nothing
#     (user override wins; useful for ephemeral / no-persistence runs).
#   * Else, ensure `/root/.latchkey/encryption_key` exists (create with
#     `mode 0600` if missing) and export its contents.
#   * If the dir is read-only and no key file exists, fall back to a
#     freshly-generated in-memory key with a loud warning — credentials
#     written under that key won't survive container restart.

set -eu

LATCHKEY_DIR="${LATCHKEY_DIR_OVERRIDE:-/root/.latchkey}"
KEY_FILE="${LATCHKEY_DIR}/encryption_key"

if [ -z "${LATCHKEY_ENCRYPTION_KEY:-}" ]; then
    mkdir -p "${LATCHKEY_DIR}" 2>/dev/null || true

    if [ -r "${KEY_FILE}" ]; then
        LATCHKEY_ENCRYPTION_KEY="$(cat "${KEY_FILE}")"
    elif [ -w "${LATCHKEY_DIR}" ]; then
        # First run against this bind mount: provision a fresh key.
        # 32 bytes of /dev/urandom, base64-encoded, no trailing newline.
        # openssl is in the base ubuntu image.
        LATCHKEY_ENCRYPTION_KEY="$(openssl rand -base64 32 | tr -d '\n')"
        umask 077
        printf '%s' "${LATCHKEY_ENCRYPTION_KEY}" > "${KEY_FILE}"
        chmod 0600 "${KEY_FILE}" || true
        echo "frankweiler-entrypoint: provisioned new latchkey encryption key at ${KEY_FILE}" >&2
    else
        # Read-only bind mount with no pre-existing key file. We can
        # still proceed — but only with an ephemeral key that won't
        # match any previously-stored .enc blobs and won't survive this
        # container.
        LATCHKEY_ENCRYPTION_KEY="$(openssl rand -base64 32 | tr -d '\n')"
        echo "frankweiler-entrypoint: WARNING — ${LATCHKEY_DIR} is read-only and has no encryption_key file." >&2
        echo "frankweiler-entrypoint: WARNING — using an ephemeral key; credentials written this run are unreadable next run." >&2
    fi

    export LATCHKEY_ENCRYPTION_KEY
fi

exec "$@"
