#!/bin/sh
# datalib container entrypoint.
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
# The image runs as uid 1000, not root. A folder an earlier image wrote
# as root, or one owned by a host user with another id, is not writable
# by it: this says so and names the one-line fix, rather than letting a
# sync fail deep inside on "permission denied" or, worse, quietly using
# a new key that cannot open the credentials already stored.
#
# Behavior:
#   * If `LATCHKEY_ENCRYPTION_KEY` is already set in the env, do nothing
#     (user override wins; useful for ephemeral / no-persistence runs).
#   * Else, ensure `/root/.latchkey/encryption_key` exists (create with
#     `mode 0600` if missing) and export its contents.
#   * If a key file exists that this user cannot read, stop: that is a
#     store an earlier, root, image made.
#   * If the dir is read-only and no key file exists, fall back to a
#     freshly-generated in-memory key with a loud warning — credentials
#     written under that key won't survive container restart.

set -eu

LATCHKEY_DIR="${LATCHKEY_DIRECTORY:-${HOME:-/root}/.latchkey}"
KEY_FILE="${LATCHKEY_DIR}/encryption_key"
DATA_DIR=/data

fix_ownership() {
    echo "datalib-entrypoint: this container runs as uid $(id -u); give it the folder once with" >&2
    echo "datalib-entrypoint:   docker run --rm --user 0 --entrypoint chown -v <host folder>:$1 <image> -R $(id -u):$(id -g) $1" >&2
}

# A data root that holds anything this user cannot write: an earlier
# image wrote it as root, or the host folder belongs to another uid. Only
# the first few levels are looked at, so a large root costs nothing. A
# read-only mount, for reading the stores back, is left alone.
read_only_mount() {
    awk -v at="$1" '$2 == at { split($4, o, ","); ro = (o[1] == "ro") } END { exit !ro }' /proc/mounts
}

if [ -d "${DATA_DIR}" ] && ! read_only_mount "${DATA_DIR}" \
    && [ -n "$(find "${DATA_DIR}" -maxdepth 3 ! -writable -print -quit 2>/dev/null)" ]; then
    echo "datalib-entrypoint: WARNING - ${DATA_DIR} holds files uid $(id -u) cannot write, so a sync there fails." >&2
    fix_ownership "${DATA_DIR}"
fi

if [ -z "${LATCHKEY_ENCRYPTION_KEY:-}" ]; then
    mkdir -p "${LATCHKEY_DIR}" 2>/dev/null || true

    if [ -e "${KEY_FILE}" ] && [ ! -r "${KEY_FILE}" ]; then
        echo "datalib-entrypoint: ${KEY_FILE} is there but uid $(id -u) cannot read it, so nothing stored in ${LATCHKEY_DIR} can be opened." >&2
        fix_ownership "${LATCHKEY_DIR}"
        exit 1
    elif [ -r "${KEY_FILE}" ]; then
        LATCHKEY_ENCRYPTION_KEY="$(cat "${KEY_FILE}")"
    elif [ -w "${LATCHKEY_DIR}" ]; then
        # First run against this bind mount: provision a fresh key.
        # 32 bytes of /dev/urandom, base64-encoded, no trailing newline.
        # openssl is in the base ubuntu image.
        LATCHKEY_ENCRYPTION_KEY="$(openssl rand -base64 32 | tr -d '\n')"
        # The umask is for the key file only. Set in this shell it
        # would leak through `exec` below, and every directory the
        # pipeline then wrote into the user's bind mount would be 0700,
        # unreadable to a host user with another uid (#469).
        (umask 077; printf '%s' "${LATCHKEY_ENCRYPTION_KEY}" > "${KEY_FILE}")
        chmod 0600 "${KEY_FILE}" || true
        echo "datalib-entrypoint: provisioned new latchkey encryption key at ${KEY_FILE}" >&2
    else
        # Read-only bind mount with no pre-existing key file. We can
        # still proceed — but only with an ephemeral key that won't
        # match any previously-stored .enc blobs and won't survive this
        # container.
        LATCHKEY_ENCRYPTION_KEY="$(openssl rand -base64 32 | tr -d '\n')"
        echo "datalib-entrypoint: WARNING — ${LATCHKEY_DIR} is read-only and has no encryption_key file." >&2
        echo "datalib-entrypoint: WARNING — using an ephemeral key; credentials written this run are unreadable next run." >&2
    fi

    export LATCHKEY_ENCRYPTION_KEY
fi

exec "$@"
