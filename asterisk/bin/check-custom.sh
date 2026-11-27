#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
# Checks the site's own Asterisk configuration ($PBX_DIR/config/asterisk, mounted read-only at
# /etc/asterisk-custom) in a throwaway container of the running image, before it is reloaded:
#   - only the supported <file>.d folders (others would never be read)
#   - no own <name>.conf that replaces a file of the image
#   - every line is a section, a setting, a comment or an include; files start with a section
#   - no section that already exists in Patchbay's file of the same name (it would be merged
#     into Patchbay's own, e.g. a context [sub-user] or a pjsip [transport-udp])
#   - the dialplan with the own files loads without errors
# Exit code 1 on errors. Run by `pbxctl asterisk-check` (deploy/pbxctl):
#   docker run --rm --network none --user root -v $PBX_DIR/config/asterisk:/etc/asterisk-custom:ro \
#     -v $PBX_DIR/generated:/etc/asterisk-generated:ro --entrypoint /usr/local/bin/check-custom.sh <image id>
set -u

C=/etc/asterisk-custom
A=/etc/asterisk
DIRS="extensions func_odbc manager ari pjsip iax confbridge musiconhold features queues modules logger"
errors=0
err() { echo "ERROR: $*"; errors=$((errors + 1)); }

if [ ! -d "$C" ] || [ -z "$(ls -A "$C" 2>/dev/null)" ]; then
    echo "config/asterisk is empty or missing: nothing to check"
    exit 0
fi

# folders and own files
SUPPORTED=$(for d in $DIRS; do printf '%s.d ' "$d"; done)
for d in "$C"/*/; do
    [ -d "$d" ] || continue
    n=$(basename "$d")
    case " $SUPPORTED" in
        *" $n "*) ;;
        *) err "config/asterisk/$n/: unknown folder, never read (supported: $SUPPORTED)" ;;
    esac
done
for f in "$C"/*.conf; do
    [ -f "$f" ] || continue
    n=$(basename "$f")
    if [ -e "$A/$n" ]; then
        err "config/asterisk/$n replaces a file of the image and is ignored; additions go into ${n%.conf}.d/"
    else
        echo "own file: $n (linked into /etc/asterisk at start)"
    fi
done

# line syntax and section names
sections() { grep -h -o '^\[[^]]*\]' "$@" 2>/dev/null | tr -d '[]' | sort -u; }
G=/etc/asterisk-generated
for d in $DIRS; do
    for f in "$C/$d.d"/*.conf; do
        [ -f "$f" ] || continue
        rel=${f#"$C"/}
        awk -v rel="$rel" -v mod="$d" '
            function bad(msg) { printf "ERROR: %s:%d: %s: %s\n", rel, NR, msg, $0; n++ }
            # block comments ;-- … --;
            inblock { if (/--;/) inblock = 0; next }
            /^[[:space:]]*;--/ { if (!/--;/) inblock = 1; next }
            /^[[:space:]]*$/ || /^[[:space:]]*;/ { next }
            /^[[:space:]]*#(try)?include[[:space:]]/ { bad("#include is not allowed here (put the file into the folder itself)"); next }
            /^\[[^]]+\](\([^)]*\))?[[:space:]]*(;.*)?$/ { seen = 1; next }
            /^[[:space:]]*[^=[:space:]][^=]*=>?/ {
                if (!seen && mod != "modules" && mod != "logger") bad("setting before the first [section]")
                next
            }
            { bad("neither a [section], a setting (name = value) nor a comment") }
            END { exit n > 0 }
        ' "$f" || errors=$((errors + 1))
        # sections of Patchbay: the image's file, the ones written at start into /etc/asterisk-site
        # (site-config.sh) and by patchbay-web into /etc/asterisk-generated (trunks, IAX peers,
        # custom apps, hints; mounted by pbxctl)
        case "$d" in
            pjsip) site="transport-udp transport-tcp transport-tls"; gen="$G/pjsip_trunks.conf" ;;
            iax) site=""; gen="$G/iax_peers.conf" ;;
            extensions) site=""; gen="$G/extensions_custom.conf $G/hints.conf" ;;
            manager | ari) site="pbxweb"; gen="" ;;
            *) site=""; gen="" ;;
        esac
        # shellcheck disable=SC2086
        own=$( (sections "$A/$d.conf" $gen; printf '%s\n' $site) | sort -u)
        sections "$f" | while IFS= read -r sec; do
            case "$d:$sec" in
                # the sections these files are meant to extend
                logger:logfiles | modules:modules) continue ;;
                extensions:app-custom-*) echo "ERROR: $rel: [$sec]: names app-custom-* belong to the custom apps of the web UI"; continue ;;
                queues:g[0-9]*) echo "ERROR: $rel: [$sec]: queues g<number> are the call groups (from the database)"; continue ;;
                musiconhold:default | musiconhold:app[0-9]* | musiconhold:pl[0-9]*)
                    echo "ERROR: $rel: [$sec]: hold music classes default, app<n> and pl<n> come from the database"; continue ;;
            esac
            if printf '%s\n' "$own" | grep -qxF -- "$sec"; then
                echo "ERROR: $rel: [$sec] already exists in Patchbay's $d configuration (it would be merged into it); use an own name"
            fi
        done > /tmp/coll.txt
        if [ -s /tmp/coll.txt ]; then
            cat /tmp/coll.txt
            errors=$((errors + $(wc -l < /tmp/coll.txt)))
        fi
        echo "checked: $rel ($(sections "$f" | wc -l) sections)"
    done
done

# load the dialplan (Patchbay's with the own files) in a bare Asterisk
mkdir -p /run/asterisk /var/run/asterisk /var/log/asterisk /var/lib/asterisk /var/spool/asterisk
printf '[modules]\nautoload = no\nload => pbx_config.so\n' > "$A/modules.conf"
printf '[logfiles]\nconsole => notice,warning,error\n' > "$A/logger.conf"
asterisk -f -n -g > /tmp/ast.log 2>&1 &
for _ in 1 2 3 4 5 6 7 8 9 10; do
    sleep 1
    asterisk -rx "core waitfullybooted" >/dev/null 2>&1 && break
done
if ! asterisk -rx "dialplan show" > /tmp/dialplan.txt 2>&1; then
    err "Asterisk did not start for the dialplan check: $(tail -n 3 /tmp/ast.log)"
else
    grep -E "ERROR|WARNING" /tmp/ast.log | grep -E "extensions|pbx_config|pbx\.c|config\.c" | grep -vE "asterisk-site|asterisk-test" \
        | sed 's/^/dialplan: /' > /tmp/dperr.txt
    if [ -s /tmp/dperr.txt ]; then
        cat /tmp/dperr.txt
        errors=$((errors + $(wc -l < /tmp/dperr.txt)))
    fi
    for f in "$C"/extensions.d/*.conf; do
        [ -f "$f" ] || continue
        sections "$f" | while IFS= read -r ctx; do
            if grep -qF "[ Context '$ctx' " /tmp/dialplan.txt; then
                echo "dialplan: [$ctx] loaded"
            else
                echo "ERROR: dialplan: [$ctx] from extensions.d/$(basename "$f") was not loaded"
            fi
        done >> /tmp/ctx.txt
    done
    if [ -s /tmp/ctx.txt ]; then
        cat /tmp/ctx.txt
        errors=$((errors + $(grep -c '^ERROR' /tmp/ctx.txt)))
    fi
fi
asterisk -rx "core stop now" >/dev/null 2>&1

if [ "$errors" -gt 0 ]; then
    echo "$errors error(s): fix them before reloading Asterisk"
    exit 1
fi
echo "OK"
