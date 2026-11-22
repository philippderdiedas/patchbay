#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
# Renders the site-specific Asterisk files from the environment into /etc/asterisk-site (tmpfs),
# then starts Asterisk ("$@"). All variables are documented in docs/configuration.md.
#
#   PBX_DEVICE_NETS_UDP   networks devices may use UDP/TCP from      (default: RFC 1918)
#   PBX_DEVICE_NETS_TLS   networks devices may use TLS from          (default: RFC 1918)
#   PBX_LOCAL_NETS        networks without NAT rewriting              (default: RFC 1918)
#   PBX_EXTERNAL_ADDRESS  public name/IP for SIP behind NAT           (default: none)
#   PBX_EMERGENCY_TRUNK   trunk for 110/112 when the database is down (default: none)
#   PBX_COUNTRY_CODE      own country calling code, e.g. 49          (required)
#   PBX_EMERGENCY_NUMBERS emergency numbers, 2-3 digits, comma-separated (default: 112)
# Secrets (created by patchbay-init in the patchbay-web container under $PBX_STATE_DIR/secrets/asterisk,
# copied to /etc/asterisk-secrets below):
#   internal_key    -> global PBX_INTERNAL_KEY, sent by the dialplan's CURL calls to patchbay-web's
#                      internal webhooks
#   asterisk_db_pw  -> res_odbc.conf (database role asterisk)
#   ami_pw          -> manager.conf (AMI user pbxweb)
#   ari_pw          -> ari.conf (ARI user pbxweb)
set -eu

# Only the state directory is mounted (PBX_STATE_DIR, default /state = the host's $PBX_DIR, see
# deploy/compose.yml). The paths below are linked into it; the own secrets are copied, because the
# host's secrets/ is root 700 and Asterisk runs as user asterisk.
ST=${PBX_STATE_DIR:-/state}
[ -d "$ST/secrets/asterisk" ] || { echo "site-config: $ST/secrets/asterisk missing (state directory not mounted, or patchbay-web has not initialised it yet)" >&2; exit 1; }
# replaces an image directory that holds no files (only empty subdirectories) by a link
link() {
    if [ -e "$2" ] && [ ! -L "$2" ]; then
        if [ -n "$(find "$2" -mindepth 1 ! -type d | head -n 1)" ]; then
            echo "site-config: $2 contains files, cannot link it to $1" >&2
            exit 1
        fi
        rm -rf "$2"
    fi
    ln -sfn "$1" "$2"
}
install -d -o asterisk -g asterisk -m 750 "$ST/asterisk" "$ST/asterisk/db" "$ST/asterisk/log"
link "$ST/keys" /etc/asterisk-keys
link "$ST/generated" /etc/asterisk-generated
link "$ST/media" /media
link "$ST/voicemail" /var/spool/pbx/vm
link "$ST/asterisk/db" /var/lib/asterisk/db
link "$ST/asterisk/log" /var/log/asterisk
install -d -o root -g asterisk -m 750 /etc/asterisk-secrets
for f in "$ST"/secrets/asterisk/*; do
    install -o root -g asterisk -m 640 "$f" /etc/asterisk-secrets/
done

OUT=/etc/asterisk-site
RFC1918="10.0.0.0/8,172.16.0.0/12,192.168.0.0/16"
mkdir -p "$OUT"

# "a, b,c" -> one value per line
list() { printf '%s\n' "$1" | tr ',' '\n' | sed 's/^[[:space:]]*//;s/[[:space:]]*$//' | grep -v '^$' || true; }

# only network notation (digits, dots, colons, slash) gets into the config
check_nets() {
    for n in $(list "$2"); do
        case "$n" in
            *[!0-9a-fA-F.:/]*) echo "site-config: $1 contains an invalid network: $n" >&2; exit 1 ;;
        esac
    done
}

UDP=${PBX_DEVICE_NETS_UDP:-$RFC1918}
TLS=${PBX_DEVICE_NETS_TLS:-$RFC1918}
LOCAL=${PBX_LOCAL_NETS:-$RFC1918}
EXT=${PBX_EXTERNAL_ADDRESS:-}
EMERG=${PBX_EMERGENCY_TRUNK:-}
# same forms as patchbay-web accepts (49, +49, 0049) -> 49
CC=$(printf '%s' "${PBX_COUNTRY_CODE:-}" | sed 's/[[:space:]]//g;s/^+//;s/^00//')
case "$CC" in ''|0*|*[!0-9]*|????*) echo "site-config: PBX_COUNTRY_CODE missing or invalid (e.g. 49)" >&2; exit 1 ;; esac
# "110, 112" -> "110-112" (FIELDNUM list in the dialplan)
EMNUM=$(list "${PBX_EMERGENCY_NUMBERS:-112}" | tr '\n' '-' | sed 's/-$//')
for n in $(list "${PBX_EMERGENCY_NUMBERS:-112}"); do
    case "$n" in [0-9][0-9]|[0-9][0-9][0-9]) ;; *) echo "site-config: invalid PBX_EMERGENCY_NUMBERS entry: $n" >&2; exit 1 ;; esac
done
[ -n "$EMNUM" ] || { echo "site-config: PBX_EMERGENCY_NUMBERS is empty" >&2; exit 1; }
check_nets PBX_DEVICE_NETS_UDP "$UDP"
check_nets PBX_DEVICE_NETS_TLS "$TLS"
check_nets PBX_LOCAL_NETS "$LOCAL"
case "$EXT$EMERG" in *[!0-9a-zA-Z.:_-]*) echo "site-config: invalid PBX_EXTERNAL_ADDRESS or PBX_EMERGENCY_TRUNK" >&2; exit 1 ;; esac

{
    echo "; Generated at container start from the environment (asterisk/bin/site-config.sh)."
    echo "[acl-devices-udp]"
    echo "deny = 0.0.0.0/0.0.0.0"
    for n in $(list "$UDP"); do echo "permit = $n"; done
    echo "[acl-devices-tls]"
    echo "deny = 0.0.0.0/0.0.0.0"
    for n in $(list "$TLS"); do echo "permit = $n"; done
} > "$OUT/acl.conf"

KEY=$(cat /etc/asterisk-secrets/internal_key 2>/dev/null || true)
case "$KEY" in *[!0-9a-fA-F]*) echo "site-config: internal_key must be hex" >&2; exit 1 ;; esac
[ -n "$KEY" ] || echo "site-config: /etc/asterisk-secrets/internal_key missing, internal webhooks will be refused" >&2
{
    echo "; Generated at container start from the environment (asterisk/bin/site-config.sh)."
    [ -n "$EMERG" ] && echo "PBX_EMERGENCY_TRUNK=$EMERG"
    echo "PBX_INTERNAL_KEY=$KEY"
    echo "PBX_COUNTRY_CODE=$CC"
    echo "PBX_EMERGENCY_NUMBERS=$EMNUM"
} > "$OUT/globals.conf"

transport() { # name protocol bind [tls]
    echo "[transport-$1]"
    echo "type = transport"
    echo "protocol = $2"
    echo "bind = $3"
    if [ "$2" = tls ]; then
        echo "cert_file = /etc/asterisk-keys/voip.crt"
        echo "priv_key_file = /etc/asterisk-keys/voip.key"
        # sslv23 = negotiate the highest version both sides support (TLS 1.3, else 1.2); OpenSSL 3
        # refuses TLS 1.0/1.1 at its default security level. tlsv1_2 would pin exactly TLS 1.2.
        echo "method = sslv23"
    fi
    for n in $(list "$LOCAL"); do echo "local_net = $n"; done
    echo "local_net = 127.0.0.0/8"
    if [ -n "$EXT" ]; then
        echo "external_signaling_address = $EXT"
        echo "external_media_address = $EXT"
    fi
    echo "tos = cs3"
    echo "cos = 3"
    echo
}
{
    echo "; Generated at container start from the environment (asterisk/bin/site-config.sh)."
    transport udp udp 0.0.0.0:5060
    transport tcp tcp 0.0.0.0:5060
    transport tls tls 0.0.0.0:5061
} > "$OUT/pjsip-transports.conf"

# secret <name>: one-line secret from the copy above; no characters that end a value in Asterisk configs
secret() {
    v=$(cat "/etc/asterisk-secrets/$1" 2>/dev/null) || { echo "site-config: /etc/asterisk-secrets/$1 missing" >&2; exit 1; }
    case "$v" in ''|*[!A-Za-z0-9._~+/=@%^*-]*) echo "site-config: /etc/asterisk-secrets/$1 is empty or has unsupported characters" >&2; exit 1 ;; esac
    printf '%s' "$v"
}
{
    echo "; Generated at container start (asterisk/bin/site-config.sh)."
    echo "username => asterisk"
    echo "password => $(secret asterisk_db_pw)"
} > "$OUT/res_odbc.conf"
# AMI classes patchbay-web needs (patchbay-web/src/ami.rs and callers):
#   read:  call, user                 events for call tracking (calltrack.rs)
#          security                   failed logins for /monitor/sip-auth (secwatch.rs)
#   write: system     PJSIPShow*, SorceryMemoryCacheExpire[Object] (no narrower class exists)
#          call       Hangup, ExtensionStateList
#          reporting  CoreShowChannels, IAXpeerlist, PJSIPShowRegistrationsOutbound
#          originate  Originate (ring test, click-to-call)
#          command    Command (dialplan/iax2/res_pjsip/moh reload, logger rotate, show commands,
#                     core restart when convenient after a certificate change - certwatch.rs)
# Not granted: config (GetConfig/UpdateConfig), dialplan, agent, user, dtmf, message, aoc, ...
{
    echo "; Generated at container start (asterisk/bin/site-config.sh)."
    echo "[pbxweb]"
    echo "secret = $(secret ami_pw)"
    echo "deny = 0.0.0.0/0.0.0.0"
    echo "permit = 127.0.0.1/255.255.255.255"
    echo "read = call,user,security"
    # security: only the failures interest patchbay-web (secwatch.rs); every REGISTER would add two more
    echo "eventfilter = !Event: SuccessfulAuth"
    echo "eventfilter = !Event: ChallengeSent"
    echo "write = system,call,reporting,originate,command"
} > "$OUT/manager.conf"
{
    echo "; Generated at container start (asterisk/bin/site-config.sh)."
    echo "[pbxweb]"
    echo "type = user"
    echo "read_only = no"
    echo "password = $(secret ari_pw)"
} > "$OUT/ari.conf"

chmod 644 "$OUT"/*.conf
# files with secrets: root and the asterisk group only
for f in globals.conf res_odbc.conf manager.conf ari.conf; do
    chgrp asterisk "$OUT/$f" 2>/dev/null && chmod 640 "$OUT/$f"
done
exec "$@"
