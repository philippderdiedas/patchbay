#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
# patchbay-init: prepares the PBX state directory before PostgreSQL, the migrations, Asterisk and patchbay-web
# start. Called by patchbay-start (the image's entrypoint) when the container starts as root; also usable
# on its own (e.g. as an initContainer). Runs as root, one-shot, idempotent:
#   - creates every missing secret; existing secrets are never regenerated
#   - rewrites the derived per-service copies (DSNs, password copies) from them
#   - creates the data directories with their owners and modes
#   - writes PostgreSQL's pg_hba.conf, creates an interim self-signed certificate if none exists
#
# State root: PBX_STATE_DIR (default /state, the host's $PBX_DIR, see deploy/compose.yml). Layout below it:
#   secrets/            root 700: master secrets (pg_password, *_db_pw, ami_pw, ari_pw, internal_key)
#   secrets/postgres/   root:999 750: pg_password copy, pg_hba.conf          -> postgres
#   secrets/migrate/    65534 700: owner DSN, role password copies          -> migrate
#   secrets/patchbay-web/    65534 700: DSN, keys, AMI/ARI/internal copies, oidc_client_secret -> patchbay-web
#   secrets/asterisk/   root:999 750: asterisk_db_pw, ami_pw, ari_pw, internal_key -> asterisk
#                       (site-config.sh renders res_odbc/manager/ari.conf from them)
#   keys/               voip.crt 644, voip.key 640 65534:999 (patchbay-web, Asterisk)
#   media/ voicemail/ generated/ backup/ acme/
# UIDs/GIDs: 65534 patchbay-web, 999 the asterisk group (Asterisk image) and the postgres user (postgres image).
#
# Environment: PBX_HOSTNAME (subject of the interim certificate). Random values come from the kernel
# CSPRNG via `openssl rand` (hex).
set -eu
umask 077

R=${PBX_STATE_DIR:-/state}
# the state root itself is traversable for the services; the subdirectories protect themselves
chmod 755 "$R"
S=$R/secrets
K=$R/keys
WEB=65534
GRP=999     # asterisk group (Asterisk image) = postgres user/group (postgres image)

log() { echo "patchbay-init: $*"; }
die() { echo "patchbay-init: $*" >&2; exit 1; }
[ "$(id -u)" = 0 ] || die "must run as root (Compose: user \"0:0\")"
[ -d "$R" ] || die "state directory $R missing (mount the host's state directory there)"

# new <file> <bytes>: random hex secret, only when the file does not exist
new() {
    [ -f "$1" ] && return 0
    openssl rand -hex "$2" > "$1.tmp" && mv "$1.tmp" "$1"
    log "created $1"
}
# put <src> <dst> <owner> <mode>: derived copy, rewritten on every run
put() {
    cp "$1" "$2.tmp" && chown "$3" "$2.tmp" && chmod "$4" "$2.tmp" && mv "$2.tmp" "$2"
}
# text <dst> <owner> <mode>: content from stdin, rewritten on every run
text() {
    cat > "$1.tmp" && chown "$2" "$1.tmp" && chmod "$3" "$1.tmp" && mv "$1.tmp" "$1"
}

# --- master secrets ------------------------------------------------------------------------------
install -d -m 700 "$S"
new "$S/pg_password" 24
new "$S/asterisk_db_pw" 24
new "$S/pbxweb_db_pw" 24
new "$S/ami_pw" 24
new "$S/ari_pw" 24
# shared secret of the internal webhooks (Asterisk dialplan -> patchbay-web 127.0.0.1:8081)
new "$S/internal_key" 24
chown root:$GRP "$S/pg_password"; chmod 640 "$S/pg_password"
chown root:root "$S/asterisk_db_pw" "$S/pbxweb_db_pw" "$S/ami_pw" "$S/ari_pw" "$S/internal_key"
chmod 600 "$S/asterisk_db_pw" "$S/pbxweb_db_pw" "$S/ami_pw" "$S/ari_pw" "$S/internal_key"
chmod 700 "$S"

# --- PostgreSQL: superuser password before the first start, client authentication -----------------
P=$S/postgres
install -d -o root -g $GRP -m 750 "$P"
put "$S/pg_password" "$P/pg_password" root:$GRP 640
# The unix socket stays inside the container (docker compose exec psql / pg_restore); TCP on loopback
# is shared with every process on the host (network_mode: host), so it needs passwords.
text "$P/pg_hba.conf" root:$GRP 640 <<'C'
# Written by patchbay-init on every start (patchbay-web image), changes here are overwritten.
# TYPE  DATABASE  USER  ADDRESS        METHOD
local   all       all                  trust
host    all       all   127.0.0.1/32   scram-sha-256
host    all       all   ::1/128        scram-sha-256
C

# --- Asterisk: raw secrets, site-config.sh renders the config snippets ----------------------------
A=$S/asterisk
install -d -m 750 "$A"
put "$S/asterisk_db_pw" "$A/asterisk_db_pw" root:$GRP 640
put "$S/ami_pw" "$A/ami_pw" root:$GRP 640
put "$S/ari_pw" "$A/ari_pw" root:$GRP 640
put "$S/internal_key" "$A/internal_key" root:$GRP 640
chown root:$GRP "$A"; chmod 750 "$A"

# --- patchbay-web (UID 65534) ---------------------------------------------------------------------------
W=$S/patchbay-web
install -d -m 700 "$W"
new "$W/cookie_key" 64
# key for secrets stored in the database (trunk passwords), AES-256-GCM
new "$W/secret_key" 32
# patchbay-web uses its own role without DDL rights (db/migrations/0001_initial.sql)
echo "postgres://pbxweb:$(cat "$S/pbxweb_db_pw")@127.0.0.1:5432/pbx" | text "$W/database_url" $WEB:$WEB 600
put "$S/ami_pw" "$W/ami_pw" $WEB:$WEB 600
put "$S/ari_pw" "$W/ari_pw" $WEB:$WEB 600
put "$S/internal_key" "$W/internal_key" $WEB:$WEB 600
# OIDC: put the client secret here. Left empty, patchbay-web refuses to start with PBX_AUTH=oidc|both.
[ -f "$W/oidc_client_secret" ] || : > "$W/oidc_client_secret"
# also files placed by the operator (mattermost_token, ...)
chown -R $WEB:$WEB "$W"; chmod 700 "$W"; find "$W" -maxdepth 1 -type f -exec chmod 600 {} +

# --- migrate (UID 65534): owner DSN and the passwords it sets on the login roles --------------------
M=$S/migrate
install -d -m 700 "$M"
echo "postgres://pbx:$(cat "$S/pg_password")@127.0.0.1:5432/pbx" | text "$M/database_url" $WEB:$WEB 600
put "$S/pbxweb_db_pw" "$M/pbxweb_db_pw" $WEB:$WEB 600
put "$S/asterisk_db_pw" "$M/asterisk_db_pw" $WEB:$WEB 600
chown $WEB:$WEB "$M"; chmod 700 "$M"

# --- TLS certificate (Asterisk SIP-TLS always, patchbay-web unless PBX_TLS=off) ---------------------------
# Comes from outside (ACME side container, cert-manager, own files); until then a self-signed one.
install -d -m 755 "$K"
if [ ! -e "$K/voip.crt" ]; then
    : "${PBX_HOSTNAME:?PBX_HOSTNAME missing (env file)}"
    case "$PBX_HOSTNAME" in *[!A-Za-z0-9.-]*) die "PBX_HOSTNAME is not a host name" ;; esac
    openssl req -x509 -newkey rsa:3072 -nodes -days 90 -subj "/CN=$PBX_HOSTNAME" \
        -addext "subjectAltName=DNS:$PBX_HOSTNAME" -keyout "$K/voip.key" -out "$K/voip.crt" 2>/dev/null
    log "created a self-signed interim certificate for $PBX_HOSTNAME"
fi
# key: owned by patchbay-web (65534), readable by the asterisk group (999)
chmod 644 "$K/voip.crt" 2>/dev/null || true
chown $WEB:$GRP "$K/voip.key" 2>/dev/null && chmod 640 "$K/voip.key" 2>/dev/null || true

# --- data directories ------------------------------------------------------------------------------
# Announcements, hold music: patchbay-web (65534) writes, Asterisk reads (all of media at /media, read-only)
install -d -o $WEB -g $WEB -m 755 "$R/media" "$R/media/asterisk" "$R/media/preview" \
    "$R/media/moh" "$R/media/moh/default"
# Voicemail messages (MP3 and phone copy): only patchbay-web and Asterisk (group 999) read them; setgid so
# new files belong to group 999. Owner, mode and the files' group are re-applied on every start (e.g.
# after a restore from a backup).
install -d -o $WEB -g $GRP -m 2750 "$R/media/voicemail"
chown $WEB:$GRP "$R/media/voicemail"; chmod 2750 "$R/media/voicemail"
find "$R/media/voicemail" -mindepth 1 -maxdepth 1 -type f -exec chgrp $GRP {} + -exec chmod 640 {} +
# a site's own initial hold music (optional; empty: patchbay-web imports the music shipped in its image)
install -d -m 755 "$R/media/moh-default"
# Voicemail spool: written by Asterisk (UID/GID 999), read and cleaned up by patchbay-web (65534, member
# of group 999 via group_add). setgid: new files belong to group 999. Modes re-applied as above.
install -d -o root -g $GRP -m 2770 "$R/voicemail"
chown root:$GRP "$R/voicemail"; chmod 2770 "$R/voicemail"
find "$R/voicemail" -mindepth 1 -maxdepth 1 -type f -exec chgrp $GRP {} + -exec chmod g+rw,o-rwx {} +
# Generated Asterisk config (custom apps, IAX peers with secrets): written by patchbay-web, read by the asterisk group
install -d -o $WEB -g $GRP -m 750 "$R/generated"
chown $WEB:$GRP "$R/generated"; chmod 750 "$R/generated"
# Database dumps (optional backup service) and acme.sh state (optional acme service)
install -d -m 700 "$R/backup" "$R/acme"
log "state in $R ready"
