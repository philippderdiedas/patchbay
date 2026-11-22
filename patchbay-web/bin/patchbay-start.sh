#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
# Entrypoint of the patchbay-web image.
#
# Without arguments, as root and with the host's state directory mounted at PBX_STATE_DIR (default
# /state = $PBX_DIR; deploy/compose.yml), on every start:
#   1. patchbay-init: missing secrets, per-service copies, pg_hba.conf, directories, permissions; the
#      paths /keys, /media, /voicemail, /generated, /config are linked into the state directory
#   2. patchbay-web's own secrets are copied into the container (the host's secrets/ is root 700)
#   3. database migrations as the owner role, waiting for PostgreSQL
#   4. patchbay-web itself, as 65534 (setpriv), without root and without the other services' secrets
# The healthcheck (/tmp/pbx-ready, written by patchbay-web once it listens) tells Asterisk when the
# database is ready.
#
# With arguments (`migrate`, `set-password`, `mint-session`, …) or as non-root, patchbay-web runs directly
# with them; the secret paths then come from the environment (the image's defaults point to the copies
# in /run/patchbay-web).
set -eu

R=${PBX_STATE_DIR:-/state}

if [ "$#" -eq 0 ] && [ "$(id -u)" = 0 ] && [ -d "$R/." ]; then
    /usr/local/bin/patchbay-init
    S=$R/secrets

    # the paths patchbay-web uses, linked into the state directory (one mount instead of six)
    for d in keys media voicemail generated config; do
        if [ -e "/$d" ] && [ ! -L "/$d" ]; then
            [ -z "$(find "/$d" -mindepth 1 ! -type d | head -n 1)" ] || { echo "patchbay-start: /$d contains files, cannot link it" >&2; exit 1; }
            rm -rf "/$d"
        fi
        ln -sfn "$R/$d" "/$d"
    done

    # patchbay-web's secrets: a private copy inside the container, owned by 65534
    W=/run/patchbay-web
    install -d -o 65534 -g 65534 -m 700 "$W"
    cp -p "$S"/patchbay-web/* "$W"/
    chown 65534:65534 "$W"/*
    export PBX_DATABASE_URL_FILE="${PBX_DATABASE_URL_FILE:-$W/database_url}"
    export PBX_COOKIE_KEY_FILE="${PBX_COOKIE_KEY_FILE:-$W/cookie_key}"
    export PBX_AMI_SECRET_FILE="${PBX_AMI_SECRET_FILE:-$W/ami_pw}"
    export PBX_ARI_SECRET_FILE="${PBX_ARI_SECRET_FILE:-$W/ari_pw}"
    export PBX_SECRET_KEY_FILE="${PBX_SECRET_KEY_FILE:-$W/secret_key}"
    export PBX_INTERNAL_KEY_FILE="${PBX_INTERNAL_KEY_FILE:-$W/internal_key}"
    if [ -f "$W/oidc_client_secret" ]; then
        export OIDC_CLIENT_SECRET_FILE="${OIDC_CLIENT_SECRET_FILE:-$W/oidc_client_secret}"
    fi

    # migrations (schema, role passwords); PostgreSQL may still be starting
    M=$S/migrate
    n=0
    until PBX_MIGRATE_DATABASE_URL_FILE=$M/database_url PBX_DB_PASSWORD_PBXWEB_FILE=$M/pbxweb_db_pw \
          PBX_DB_PASSWORD_ASTERISK_FILE=$M/asterisk_db_pw /usr/local/bin/patchbay-web migrate; do
        n=$((n + 1))
        [ "$n" -lt 90 ] || { echo "patchbay-start: migrations failed, giving up" >&2; exit 1; }
        echo "patchbay-start: waiting for PostgreSQL ($n)"
        sleep 2
    done

    # supplementary groups without root's group 0 (keeps e.g. 999 from group_add: voicemail spool)
    groups=$(id -G | tr ' ' '\n' | grep -vx 0 | paste -sd, -)
    if [ -n "$groups" ]; then
        exec setpriv --reuid=65534 --regid=65534 --groups="$groups" /usr/local/bin/patchbay-web
    fi
    exec setpriv --reuid=65534 --regid=65534 --clear-groups /usr/local/bin/patchbay-web
fi

exec /usr/local/bin/patchbay-web "$@"
