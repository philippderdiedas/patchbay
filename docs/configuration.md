# Configuration

Code, Asterisk configuration, templates and migrations contain no site-specific values. A site is
configured entirely through environment variables in an env file that Compose loads into the
containers (`env_file`, default `.env` next to the compose file). Secrets are files,
never variables (see below).

**All variables, with defaults and descriptions: [`deploy/pbx.env.example`](../deploy/pbx.env.example).**
Copy it as the env file, fill in the required ones and uncomment what you change. This document
explains how to operate and configure a site; it does not repeat the list. How the system works
inside: [`architecture.md`](architecture.md).

## Deployment

An operator needs only the two images, one Compose file ([`deploy/compose.yml`](../deploy/compose.yml),
self-contained, no repository checkout) and the env file. Everything else – secrets, directories,
the database schema – the containers create themselves. No host scripts, no systemd units.

```sh
mkdir pbx-site && cd pbx-site                     # any directory
cp compose.yml docker-compose.yml
cp pbx.env.example .env && $EDITOR .env           # required: PBX_HOSTNAME, PBX_COUNTRY_CODE
docker compose up -d                              # [--profile acme] [--profile backup]
```

**Services:** `postgres`, `patchbay-web`, `asterisk`; optional `acme` and `backup` (profiles).
`patchbay-web` starts as root and, on every start, runs `patchbay-init` (missing secrets, directories,
permissions, an interim certificate), applies the database migrations (waiting for PostgreSQL) and
then runs as an unprivileged user (`patchbay-web/bin/patchbay-start.sh`). PostgreSQL waits for the secrets
`patchbay-init` writes; Asterisk waits until patchbay-web is healthy (migrated and listening). After a host
reboot Docker restarts the containers by itself.

**One env file for everything:** `.env` next to the compose file is loaded into the containers
(`env_file`) and read by Compose itself for its variables (`PBX_DIR`, `PBX_POSTGRES_DIR`,
`PBX_VERSION`, `PBX_IMAGE_PREFIX`, `PBX_ENV_FILE`, `COMPOSE_PROFILES`; section “Compose” of the
example). Compose interpolates `$` in `.env`; write `$$` for a literal dollar sign. Another name:
`PBX_ENV_FILE=pbx.env` (then Compose's own variables go into the shell environment).

**State directory** (`$PBX_DIR`, default `./pbx`; patchbay-web and Asterisk mount only this directory and
link their paths into it):

| Path | Owner, mode | Content |
|---|---|---|
| `config/` | operator | optional `theme.css`, `logo.svg`/`logo.png`, `icon-32.png`, `icon-180.png` (see Site and web interface) |
| `secrets/` | root 700 | master secrets: `pg_password`, `pbxweb_db_pw`, `asterisk_db_pw`, `ami_pw`, `ari_pw`, `internal_key`; `dns_api_token` (placed by hand, only for `acme`) |
| `secrets/postgres/` | root:999 750 | `pg_password` copy, `pg_hba.conf` → postgres, backup |
| `secrets/migrate/` | 65534 700 | owner DSN, role password copies → migrations |
| `secrets/patchbay-web/` | 65534 700 | DSN, `cookie_key`, `secret_key`, AMI/ARI/internal key copies, `oidc_client_secret` (and e.g. `mattermost_token`, placed by hand) → patchbay-web |
| `secrets/asterisk/` | root:999 750 | `asterisk_db_pw`, `ami_pw`, `ari_pw`, `internal_key` → Asterisk |
| `keys/` | 755 | `voip.crt` (644), `voip.key` (640 65534:999) |
| `media/` | 65534 755 | announcements, hold music, voicemail audio, cached speech (`tts/`) |
| `voicemail/` | root:999 2770 | voicemail spool (Asterisk writes, patchbay-web reads and deletes) |
| `generated/` | 65534:999 750 | Asterisk config written by patchbay-web (custom apps, IAX peers, hints) |
| `asterisk/` | asterisk 750 | astdb (`db/`) and Asterisk's log files (`log/`) |
| `backup/`, `acme/` | root 700 | dumps of the `backup` service, acme.sh state |

PostgreSQL's data directory is `$PBX_POSTGRES_DIR` (default `./postgresql`). A part elsewhere (big
logs, media on a NAS): mount it over the subdirectory in both services, e.g. in
`docker-compose.override.yml` `- /mnt/nas/media:/state/media`; moving existing data is up to you.

Each running service sees only its own secrets: the start scripts (root) copy them into the
container before dropping privileges, the host's `secrets/` stays root 700.

### patchbay-init (state setup)

`patchbay-init` (`patchbay-web/bin/patchbay-init.sh`) creates every missing secret (`openssl rand`, kernel CSPRNG;
existing files are never regenerated), rewrites the per-service copies and DSNs from them, writes
`secrets/postgres/pg_hba.conf` (PostgreSQL runs with `-c hba_file=` pointing to it: passwords
(SCRAM) on TCP loopback, which every process on the host shares because of host networking; `trust`
only on the unix socket inside the container), creates the directories with their owners and modes
(and fixes the group/modes of existing voicemail files), and creates a self-signed interim
certificate for `PBX_HOSTNAME` when `keys/voip.crt` does not exist.

The Asterisk credential snippets (`res_odbc.conf`, `manager.conf`, `ari.conf`) are rendered by the
Asterisk image itself at container start (`site-config.sh`) from the raw secrets in
`secrets/asterisk/`. The AMI user `pbxweb` has only the classes patchbay-web uses (read `call,user`;
write `system,call,reporting,originate,command`), see the comment in `site-config.sh`.

New hold music on a new site: patchbay-web imports the files of `media/moh-default` (a site's own
`.wav`/`.sln16` files) or, when that is empty, the Opsound music shipped in its image as the default
hold music, once.

### Images

The images contain the complete, site-neutral code and configuration; nothing from the repository
is mounted at runtime.

- `patchbay-asterisk` (`asterisk/Dockerfile`, context `asterisk/`): Asterisk built from source
  (`ARG ASTERISK_VERSION`, image label `pbx.asterisk.version`), `asterisk/etc` as `/etc/asterisk`,
  German prompts generated with Piper TTS (`asterisk/tts`), `site-config.sh` (the image's default
  command: links the paths into the state directory, renders `/etc/asterisk-site`, starts Asterisk
  as user asterisk).
- `patchbay-web` (`patchbay-web/Dockerfile`, context: repository root): the web UI; `db/migrations` are
  embedded at build time; `patchbay-init`, `patchbay-start` (entrypoint); the initial hold music.

`deploy/build.sh [--context <docker-context>] [--tag <tag>] [--only asterisk|web] [-- <docker build args>]`
builds both images with tag `git describe --always --dirty` (`dev` outside Git) and also tags
`:latest`. `--context` builds on another Docker daemon, e.g. directly on the PBX host. To move
images instead: `docker save patchbay-asterisk:<tag> patchbay-web:<tag> | ssh <host> docker load`, or push
them to a registry and set `PBX_IMAGE_PREFIX`.

### Migrations

patchbay-web applies the embedded `db/migrations/*.sql` at every start (`patchbay-web migrate`, run by
`patchbay-start` as the owner role `pbx` before patchbay-web itself starts): in name order, skipping those
recorded in `schema_migrations`; each file and its row are committed in one transaction (a failed
migration leaves no trace and is retried next time). A PostgreSQL advisory lock serialises
concurrent runners. Afterwards it sets the passwords of the login roles `pbxweb` and `asterisk` from
the secret files (as SCRAM verifiers; unchanged passwords are left alone). A newer database than the
image (e.g. after a rollback) is only logged as a warning. Asterisk caches devices in memory;
patchbay-web empties these caches at every start, so schema or data changes apply even when the Asterisk
container kept running.

## Site and web interface (patchbay-web)

Variables: [`deploy/pbx.env.example`](../deploy/pbx.env.example).

**Look of a site** (files in `$PBX_DIR/config/`, picked up without a restart or a variable):

| File | Replaces |
|---|---|
| `theme.css` | colours and styles (CSS variables of `static/base.css`); `PBX_THEME_CSS` only for another path or `off` |
| `logo.svg` or `logo.png` | the logo in the navigation (24 px) and on the login page (96 px), and the favicon of current browsers |
| `icon-32.png` | the favicon of older browsers and `/favicon.ico` |
| `icon-180.png` | the home screen icon (iOS) |

Missing files keep Patchbay's defaults. Site images are served with a sandboxing
Content-Security-Policy, so an SVG cannot run script even when opened directly.

## CardDAV (phone books)

patchbay-web serves CardDAV under `<PBX_PUBLIC_URL>/carddav/` (`/.well-known/carddav` redirects there).
Login: a device's SIP credentials (Digest, read-only; provisioned Linphone uses these) or an app
password created under *Phone book* (user `dav<user id>`, read-write for personal contacts).
PROPFIND on the home set lists the address books:

| Collection | Access | Content |
|---|---|---|
| `/carddav/colleagues/` | read-only | Persons listed in the phone book (`PBX_PHONEBOOK`), extension and SIP address. |
| `/carddav/contacts/` | read-write (app password) | The user's personal contacts, cards stored verbatim. |
| `/carddav/services/` | read-only | "Services": the emergency numbers (`PBX_EMERGENCY_NUMBERS`, e.g. "Emergency 112"), the feature codes without an argument (`*8`, `*761`, `*760`, `#21#`, `*97`, `*43`; codes with an argument such as `*21*<number>#` or `*8<extension>` cannot be stored as a contact), all call groups, voice menus with a number and enabled company apps and custom apps – the same list as the help page. |
| `/carddav/book/` | read-only, not listed | All three combined; Linphone's remote contact directory (search). |

Service cards carry `ORG` and `CATEGORIES` "Services" (so clients group them), the kind as `TITLE`,
`TEL` plus `IMPP:sip:<number>@<realm>` (`#` percent-encoded) and stable UIDs (`svc-<number>`,
`*` → `s`, `#` → `h`, e.g. `svc-s97`). Names are in the user's UI language (web setting, else the
client's `Accept-Language`), e.g. "Mailbox (*97)" / "Voicemail (*97)". ETags are content hashes and
the collection's `getctag` changes whenever an entry is added, removed, renamed or the language
changes, so clients sync incrementally. The feature-code list lives in `patchbay-web/src/help.rs`
(`FEATURE_CODES`) and drives both the help page and this address book. Feature codes only work
from the user's own device (the dialplan identifies the caller by its SIP endpoint).

The **shared phone book** (`PBX_SHARED_CONTACTS=off|read|write`, default off) is a fourth book,
`/carddav/shared/`, for contacts everyone with phone access should have (suppliers, customers,
service numbers outside the PBX). `read`: everyone sees it, only admins create, change and delete
entries (web UI). `write`: everyone with phone access edits it, in the web UI and over CardDAV with an
app password. Each entry shows who created and last changed it; changes are in the audit log. Deleting
moves an entry to a bin for 30 days, where admins restore it (phone book page); housekeeping removes it
afterwards. Own contacts can be moved there, shared ones copied to the own contacts. Caller names use
the person's own contacts first, then the shared phone book. It is also part of `/carddav/book/`.

**Duplicates** (phone book → "Find duplicates"): compares the person's own contacts with each other
and with the shared phone book by phone number (E.164; internal numbers as digits), e-mail and similar
names (same words, or up to two typos), each switchable. Per pair: merge (choose each field and the
entry that remains; the other is deleted, a shared one goes to the bin), delete one, or "not a
duplicate" (remembered per person, undone on the same page). Merging needs edit rights on both entries.

**External phone books** (`PBX_EXTERNAL_BOOKS`, see `deploy/pbx.env.example`): read-only CardDAV
address books of other systems, e.g. an ERP or CRM; for Dolibarr there is
[doli-carddav-bridge](https://git.fablab-luebeck.de/fablab-public/doli-carddav-bridge). patchbay-web
loads each one every few minutes (ctag check, then only new or changed cards) into a local copy, so
caller names keep working while the other system is down. They appear on the phone book page, as
`/carddav/ext-<id>/` (also in `/carddav/book/`), in the call window's search, in "My calls" and for
caller names after the person's own contacts and the shared phone book. A card's `URL` (http/https)
becomes an "open in …" link. Changes are made in the other system.

Force a resync on all clients (e.g. a Linphone that keeps an empty list): change the setting
`carddav_epoch`, e.g. `INSERT INTO settings VALUES ('carddav_epoch', now()::text) ON CONFLICT (key) DO
UPDATE SET value = EXCLUDED.value;`. All collection tags (ctag) change; clients compare the etags again and
load what they miss.

## Public holidays

Time profiles with public holidays switched on use the site's holiday calendar (Admin → Time
profiles → *Holiday calendar*). It is empty on a new site; nothing is generated automatically.
Admins add holidays by hand or import them for the current and the next year:

- **Nager.Date**: country (ISO code) and optionally a subdivision (ISO 3166-2, e.g. `DE-SH`); without a
  subdivision only nationwide holidays are imported. API base: `PBX_HOLIDAY_NAGER_URL`.
- **iCal**: URL or uploaded `.ics` file. Single and all-day events (multi-day ones as several days)
  and simple yearly repetitions are imported; other repeating events are skipped and counted.

*Preview* shows what would be imported. *Keep updated* (stored per source in the database; URL
sources only) re-imports the source daily: only holidays of that source are replaced, manual entries
win on the same date, edited imported holidays become manual, deleted imported holidays are not
imported again (*Restore* brings them back). If a source cannot be fetched, its holidays stay and the
error is shown next to the source. Calls are only ever routed from the stored calendar.

## Number plan

Every internal number has exactly 4 digits (1000–9999) and exists only once across all kinds
(persons, devices, call groups, voice menus, apps); the database enforces this. Per kind a range
and an allocation strategy can be set:

`<first>-<last>[:strategy]` or `pool[:strategy]` (the whole 1000–9999, shared with other kinds);
strategy `next` (next free, default), `random` or `choose` (the user picks a free number; for
persons on the start page, for personal apps when creating them). Admins can always enter any free
number in the range. Changing a range does not renumber anything: patchbay-web refuses to start while
existing numbers lie outside their range.

Variables: [`deploy/pbx.env.example`](../deploy/pbx.env.example).

Other sites are dialled as their IAX prefix + 4-digit number. The prefix belongs to the trunk (how our
users reach that peer); a site has no prefix of its own, and caller IDs go to the peer as plain
extensions (the peer puts its prefix for us in front). Each IAX trunk also names "our login at the
peer", the name the peer has given this PBX. The dial rules are not tied to the
number plan; Admin → Permissions & routes has a tester that shows which way a number would take.
Phones are expected to send the complete number (no digit-by-digit dialling).

Example with fixed blocks: `PBX_NUMBERS_PERSON=1000-1999`, `PBX_NUMBERS_DEVICE=2000-3999`,
`PBX_NUMBERS_GROUP=5000-6999`, `PBX_NUMBERS_IVR=7000-7999`, `PBX_NUMBERS_COMPANY=8000-8999`,
`PBX_NUMBERS_APP=9000-9999`. Example "we don't care": `PBX_NUMBERS_PERSON=pool:choose`,
`PBX_NUMBERS_DEVICE=off`, everything else unset.

## Login (OIDC / Keycloak and local accounts)

Variables: [`deploy/pbx.env.example`](../deploy/pbx.env.example).

The client secret is the file `oidc_client_secret` (only needed with OIDC). With `PBX_AUTH=oidc` or
`both`, patchbay-web refuses to start while that file is empty.

**Permissions.** Every user has three switches in Admin → Users → edit: *Web interface* (may log
in), *Administrator* and *Phone access* (`users.phone_allowed`, what Asterisk checks). They combine
with the OIDC groups as follows:

| | Local login | OIDC login |
|---|---|---|
| Web interface | switch *Web interface* or *Administrator* | group `PBX_GROUP_WEBUI`/`PBX_GROUP_ADMIN` **or** the switches (linked users) |
| Admin | switch *Administrator* | group `PBX_GROUP_ADMIN` **or** the switch |
| Phone | switch *Phone access* | `PBX_OIDC_SYNC=keycloak`: group `PBX_GROUP_PHONE` (the sync sets the switch, it is read-only in the form); `off`: the switch |

The switches default to off: with OIDC groups nothing has to be set, local logins need them.
Phone access applies to the person, whichever way they log in.

**Removing people with `PBX_PROVISION=auto`.** The sync (and every login) creates a person for each
member of `PBX_GROUP_PHONE` that has none, so deleting or unlinking such a person here would be
undone. With the Keycloak sync on, patchbay-web refuses both while the stored groups of the account
(refreshed by every sync and login) contain the phone group. Remove the account from the group in
Keycloak instead: the next sync (every 10 minutes, or *Sync with Keycloak now*) revokes phone access
and logs out the devices. The person itself stays (locked, with its number) until an admin deletes it,
which is then allowed. Without the sync (`PBX_OIDC_SYNC=off`) nothing is refused: the stored groups
are only as fresh as the account's last login.

**Local accounts.** One local login per user: login name (2–64 characters `a–z 0–9 . _ - @`,
case-insensitive) and an argon2id password hash (table `local_logins`, not readable by the Asterisk
role). Admins create logins, rename them and set passwords in Admin → Users → edit → *Local login*;
users change their own password on the start page (at least 10 characters). A new password ends
all other sessions of that login. Failed logins are rate limited in memory: 5 failures within
15 minutes lock the login name for 5 minutes (doubling with each further lockout, at most 1 hour),
20 failures from one IP lock that IP for 15 minutes (at most 1 hour); unknown login names take as
long as known ones.

First admin (no admin yet, e.g. a new site with `PBX_AUTH=local`), in the directory of `compose.yml`:

```sh
docker compose exec -T patchbay-web patchbay-web set-password auto --create "Jane Admin" --admin [--login <name>] < pwfile
# existing person: docker compose exec -T patchbay-web patchbay-web set-password <extension|login> [--login <name>] [--admin] < pwfile
```

The password is the first line of stdin. `--create "<name>"` first creates the person (extension
`auto` = next free number of the number plan, or a given 4-digit extension). The login name defaults
to the extension; the command also switches on *Web interface* (and with `--admin` *Administrator*)
and writes the audit log (actor `system:cli`). The optional host script `deploy/pbxctl` wraps this
and a few user/device commands; nothing depends on it.

For browser automation (UI tests without single sign-on), `docker compose exec patchbay-web
patchbay-web mint-session [minutes] [user-id]` prints a short-lived admin session cookie (`pbx_session`,
default 60, at most 240 minutes); with a user id the session also acts as that person. Its actions
appear in the audit log with the actor `automation:cli`.

## Apps

Apps are internal numbers without hardware: conference, music, announcement, echo, speaking clock,
call group, script, alias and dial-through ([architecture](architecture.md#7-apps)).

**Who creates them.** Admins create *organization apps* under Admin → Apps (number range
`PBX_NUMBERS_COMPANY`, every kind) and custom dialplan apps. Persons with phone access create
*personal apps* on their start page (range `PBX_NUMBERS_APP`, at most `PBX_APPS_PER_USER`) of the
kinds `PBX_USER_APPS` allows: `true` = the safe kinds (no script, no dial-through), `false` = none,
or an explicit list. `script` must be listed explicitly; organization-only kinds (`gateway`,
dial-through) in the list are a startup error. Personal apps of a kind that is no longer allowed
stop working, are marked on the start page and can only be deleted.

**Apps as call targets.** The target selectors of own numbers (DIDs), trunk inbound rules, forwards
and no-answer rules offer "App" for organization apps. An app that is a target cannot be deleted; the
message lists the numbers, rules and forwards to change first. The call flow diagram shows a
switched-off app target as not active.

**Dial-through** (organization only): PIN (3–8 digits, required) and a prefix (1–8 digits; saving
checks that a dial rule routes it). A caller enters the PIN, then a number ending with `#`; the PBX
dials prefix + number with a one-shot permission for exactly that number. Choose the prefix
narrowly (e.g. another site's prefix, never `0`): whoever knows the PIN can dial anything behind it.
After 5 wrong PINs within 15 minutes the app is locked for 15 minutes (shown in monitoring, service
`PBX App PINs`, and in the audit log as `app.pin_failed`). Unlock early:
`docker compose exec asterisk asterisk -rx "database deltree pbx_pinlock"`.

**Conferences:** room PIN and moderator PIN are optional and stored in plain text (Asterisk must
compare them; they are no PBX credentials). Give every conference reachable from an external
number a room PIN. Wrong PINs are logged and counted, but rooms are never locked.

**Script apps** need a Wyoming TTS server (`PBX_TTS`, e.g.
[`deploy/examples/compose.tts.yml`](../deploy/examples/compose.tts.yml)) and voices per language
(`PBX_TTS_VOICE_DE`, `PBX_TTS_VOICE_EN`). `PBX_SCRIPT_HTTP_ALLOW` restricts where `http_get()` may
fetch from; internal networks are refused unless listed. Script source is limited to 20 KB; the app
page has a test run that shows the spoken text and `print()` output.

**Custom dialplan apps** (Admin → Apps): dialplan written as MiniJinja templates (data: users,
groups, IVRs, sounds, site), each rendered into its own context `[app-custom-<id>]` and re-rendered
every minute. `PBX_CUSTOM_DIALPLAN_GUARD=strict` (default) allows only listed applications,
functions and jump targets; it catches mistakes, it is not a security boundary.

**Media:** uploads (MP3, M4A, OGG, WAV, FLAC; at most 25 MB per file, 0.2 s to 30 min) are converted
on upload; each person has a quota of `PBX_MEDIA_MAX_MB`. Music played to external callers (hold
music, music apps on a public number) is a public performance: use only freely licensed music
there. Personal hold music playlists need `PBX_USER_MOH`.

## Asterisk (rendered at container start by `asterisk/bin/site-config.sh`)

Variables: [`deploy/pbx.env.example`](../deploy/pbx.env.example).

Trunks (SIP providers, IAX connections to other sites) are managed in the web interface
(Admin → Trunks), not in configuration files. Inbound mode per SIP trunk: **DID** (called number →
`dids` → time profile → target) or **passthrough** (called number → trunk inbound rules, like IAX:
internal with allowed kinds, fixed target, transit outbound, reject; a new passthrough trunk gets one
rule "internal, persons and groups"). In both modes the admin blocklist (Admin → Blocklist) is
checked first and answers busy; IAX connections are not checked. Codecs per SIP trunk: G.722, A-law, µ-law, Opus. A target
(DID, inbound rule, forward) can be any app directly (target kind "App").

### Example: several accounts at one provider (trunk by the caller's number)

A dial rule (Admin → Permissions & routes) lists up to five trunks in the order they are tried. The
next one is tried only when a trunk could not take the call – unreachable, congested or at its call
limit, before anything rang – never after ringing, busy, no answer or a rejection of the number
(details: [architecture](architecture.md#outbound-route-classes-and-dial-rules)). With caller ID
"user's number, else route number" the trunk that owns the caller's outbound number (Admin →
person → Caller ID; the number's trunk under Numbers) is tried first, if the rule lists it (not
for emergency calls).

Many providers show only the account's own number and replace any other caller ID. Mark such SIP
trunks with **Shows only its own numbers** (Admin → Trunks → trunk → Options): a number of another
trunk is then not sent over it; the rule's number is used if it belongs to that trunk, otherwise
none (the provider shows its own).

Three accounts `A`, `B`, `C` at one provider, each with its own number; one person has `A`'s number
as outbound number, everyone else has none:

1. Trunks `A`, `B`, `C` with **Shows only its own numbers**; numbers: each account's number on its trunk.
2. The person: Caller ID = `A`'s number. The others: "Route default".
3. Dial rule "External" (prefix `0`): trunks 1. `B`, 2. `A`; caller ID "user's number, else route
   number" (route number empty or `B`'s number).

The person calls out over `A` showing `A`'s number and, when `A` is unreachable, over `B` showing
`B`'s; everyone else goes out over `B` and falls back to `A` (showing `A`'s number) when `B`
cannot take the call. The emergency
rule keeps its own trunks. The route tester (Permissions & routes, with a caller) shows the trunk
order, why a trunk comes first and the caller ID per trunk. It, the person's page and the call flow
warn when a person's number belongs to a trunk that shows only its own numbers but is switched off
or in no dial rule that dials out showing the person's number (the number would never be shown).

### Example: ISDN bridge and passthrough trunks

A second Asterisk that bridges ISDN (e.g. a payphone on a BRI) connects **like a phone**, not as a
trunk:
- One device per extension on this PBX (Admin → person → SIP device, credentials shown once).
- On the bridge, one outbound registration per device with `line = yes` and the matching endpoint,
  and a **distinct `contact_user`** per registration, so the bridge can tell inbound calls for
  the different extensions apart.
- Calls from the bridge authenticate as the device and run with that person's permissions. Over
  UDP/TCP the bridge must be in `PBX_DEVICE_NETS_UDP` (or use TLS).

A **passthrough SIP trunk** (e.g. Eventphone, where every extension of the account arrives on the
same trunk) gets one inbound rule per extension (matched after leading zeros of the called number
are stripped): prefix = that extension, min/max length = its
length, action "target" → person, call group, IVR or app (e.g. a music app), priority below 50
(rules are tried by priority first, then longest prefix). The default rule "internal" (priority 50)
can stay as fallback or be removed.

### Devices with a fixed address

Devices that never register and only wait for calls, e.g. Home Assistant's VoIP integration: Admin →
person → device type "SIP device with a fixed address", address `sip:<user>@<host>:<port>` (Home
Assistant: `sip:42@<host>:5070`; it speaks Opus only, which the PBX transcodes). The PBX calls the
address directly, without registration and without qualify (Home Assistant does not answer OPTIONS).

Voice assistants (LiveKit Agents, Pipecat, Jambonz, hosted voice agents, the OpenAI Realtime SIP
connector, Home Assistant) are connected the same way, as a SIP endpoint: a device with a fixed
address, or an IP-based SIP trunk with dial and inbound rules when the assistant also calls out.
Every call to a device carries the call context as SIP headers: `X-PBX-Caller` (number),
`X-PBX-Caller-Name` (as shown on phones, incl. phone book names and group prefixes) and `X-PBX-DID`
(the own number the call came in on, E.164; missing for internal calls). Phones ignore them.

### Mobile phone as a device

A person's mobile phone can be added as a device of type "Mobile phone": it rings in parallel with
their SIP devices (with its own on/off switch and ring delay on the start page) for calls to the
person. It has no SIP account and no direct number; the PBX calls the mobile number over the trunks
(`ring-mobile` → `sub-outbound`) within the person's own dialling permissions, checked when the
device is added and again on every call. Call groups and click-to-call do not ring it; an immediate
forward still takes precedence. The number shown on the mobile follows the dial rule and trunk, as
for forwards.

`PBX_MOBILE_DEVICES=admin` (default): only admins add mobile phones (Admin → person → device);
`users`: people add their own on the start page. Each call that rings a mobile uses a trunk channel.
If the mobile's own voicemail picks up first, the call counts as answered and does not reach the
PBX voicemail; the forms warn about this.

The TLS transport (5061) negotiates TLS 1.3 or 1.2 (`method = sslv23`; OpenSSL refuses older
versions). Devices get mandatory SRTP only over TLS; over UDP/TCP the web UI stores "SRTP if
possible" instead (SDES keys would travel in clear text).

Logs: Asterisk's console (verbose call trace) goes to `docker logs`; all containers use the
json-file driver limited to 5 × 20 MB (`deploy/compose.yml`). `messages.log` (notice, warning,
error) and `security.log` in the `logs` volume are rotated daily by patchbay-web (`logger rotate`),
seven generations are kept.

## Certificates and reverse proxy

Certificate acquisition is not part of the images. Asterisk always needs a certificate for SIP-TLS
(5061) as files in `$PBX_DIR/keys` (`voip.crt` full chain, `voip.key`): a web reverse proxy cannot
terminate SIP-TLS. Until a real certificate is there, `patchbay-init` creates a self-signed one. Whatever
writes the files – the optional `acme` service, cert-manager (a Secret mounted there), an own ACME
client – patchbay-web notices a changed `voip.crt` (`PBX_ASTERISK_CERT`, checked every 5 minutes, and at
its start when the file is newer than Asterisk's start) and restarts Asterisk in place when no call is
active (`core restart when convenient` via AMI class `command`, forced after
`PBX_ASTERISK_RESTART_MAX_WAIT`). Write the key before the certificate and keep the modes: key 640
owner 65534 group 999, certificate 644.

**a) patchbay-web serves HTTPS itself** (default, `PBX_TLS=on`): it uses the same files and reloads them
hourly.

**b) Reverse proxy** (Traefik, Caddy, nginx, Kubernetes Ingress) terminates HTTPS for the web UI:
`PBX_TLS=off`, patchbay-web listens with plain HTTP on `PBX_LISTEN` (default `0.0.0.0:8080`; with host
networking that port is reachable from the network, so bind it to `127.0.0.1:8080` when the proxy
runs on the host network, or firewall it). `PBX_PUBLIC_URL` stays the public `https://` URL: all
absolute URLs (OIDC redirect, provisioning links and QR codes, CardDAV address, bot callbacks,
mails) are built from it, never from `Host`/`X-Forwarded-*` headers, and the CSRF check compares the
browser's `Origin` with it. Session cookies stay `Secure` (the browser still talks HTTPS). Set
`PBX_TRUSTED_PROXIES` to the proxy's address so the login rate limits see the client's address from
`X-Forwarded-For`. The proxy must pass through `/prov/…`, `/carddav/…`, `/.well-known/carddav` and
`/bot/…` unchanged (no path rewriting, request bodies and the `Authorization` header intact). The
SIP certificate files are still needed (a), e.g. from the `acme` service.

Caddy on the same host (`network_mode: host`), env file `PBX_TLS=off`,
`PBX_LISTEN=127.0.0.1:8080`, `PBX_TRUSTED_PROXIES=127.0.0.1`:

```
voip.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

Traefik (file provider, Traefik on the host network):

```yaml
http:
  routers:
    pbx: { rule: "Host(`voip.example.com`)", service: pbx, tls: { certResolver: le } }
  services:
    pbx: { loadBalancer: { servers: [ { url: "http://127.0.0.1:8080" } ] } }
```

### Optional service `acme` (Let's Encrypt, DNS-01)

`docker compose --profile acme up -d` (or `COMPOSE_PROFILES=acme`). Uses the pinned
`neilpang/acme.sh` image with its state in `$PBX_DIR/acme`: issues the certificate for
`PBX_HOSTNAME` once (EC-256), then checks daily and renews when due, and installs a changed
certificate as `keys/voip.crt`/`voip.key` (modes as above). patchbay-web picks it up as described.

Variables: [`deploy/pbx.env.example`](../deploy/pbx.env.example).

A zone without DNS API can delegate `_acme-challenge` by CNAME (acme.sh `--challenge-alias`; issue
once by hand with `docker compose run --rm --entrypoint acme.sh acme --issue … --challenge-alias …`).

## Database backup (optional service `backup`)

`docker compose --profile backup up -d` (or `COMPOSE_PROFILES=backup`) runs a container of the
PostgreSQL image that writes `pg_dump -Fc` of the database daily to
`$PBX_DIR/backup/pbx-<date>-<time>.dump` (mode 600) and deletes older dumps. It connects via TCP to
127.0.0.1 with the owner password from `secrets/postgres/pg_password`.

```sh
docker compose run --rm backup now                       # immediate dump
docker compose exec -T postgres pg_restore -U pbx -d pbx --clean --if-exists < pbx/backup/<file>
```

Restore into the running stack, then `docker compose restart asterisk patchbay-web`. The dumps stay on
the PBX host: copy `$PBX_DIR/backup` (and `$PBX_DIR/secrets`, without which encrypted trunk
passwords cannot be decrypted) to off-host storage with the site's backup system.

Variables: [`deploy/pbx.env.example`](../deploy/pbx.env.example).

## Chat bot (patchbay-web, crate patchbay-notify)

The bot is off unless a chat platform is configured. It posts missed calls and voicemails to
chats, shows the calls of linked call groups in channels and takes commands (`/pbx help`).
Call tracking (AMI) runs always; its events appear in the patchbay-web log (`event {...}`).

Variables: [`deploy/pbx.env.example`](../deploy/pbx.env.example).

Mattermost setup (System Console / Integrations):
- Integrations → *Enable Bot Account Creation* and *Enable Custom Slash Commands* on.
- Bot account (Integrations → Bot Accounts), role *Member*, with an access token, added to the
  team(s) whose members use it. To link users
  automatically by e-mail (`auto`), the bot must see e-mail addresses (Site Configuration →
  Users and Teams → *Show Email Address* on); otherwise use `link`.
- Slash command `/pbx` per team: request URL `<PBX_BOT_CALLBACK_URL>/bot/mattermost/command`,
  method POST; its token goes into `PBX_BOT_MATTERMOST_SLASH_TOKEN_FILE`.
- Mattermost must be allowed to call patchbay-web: Environment → Developer → *Allow untrusted internal
  connections to* the PBX host name (buttons and slash commands go to an internal address).
- Call groups: a PBX admin invites the bot into a channel and runs `/pbx link-group <number>`
  there (`/pbx unlink-group <number>` to stop).

## Monitoring

`GET /monitor` lists the checks in the Checkmk local check format; `GET /metrics` gives the same
values for Prometheus. Both are only answered for `PBX_MONITOR_NETS`.

| Service (Checkmk) | State | Prometheus |
|---|---|---|
| `PBX Database` | CRIT when PostgreSQL does not answer | `pbx_up{component="database"}` |
| `PBX Asterisk` | CRIT when the Asterisk Manager does not answer; uptime as metric | `pbx_up{component="asterisk"}`, `pbx_asterisk_uptime_seconds` |
| `PBX Trunk <name>` (each enabled trunk) | CRIT when a registration trunk is not registered or an IAX peer is unreachable; IP trunks only when Asterisk qualifies them | `pbx_trunk_up{trunk,kind}` |
| `PBX Certificate` | WARN below 21 days, CRIT below 7 (`PBX_ASTERISK_CERT`, else the web certificate) | `pbx_certificate_days_left` |
| `PBX Devices` | always OK, registered/total as metrics | `pbx_devices_registered`, `pbx_devices_total` |
| `PBX Calls` | always OK, active channels as metric | `pbx_active_channels` |
| `PBX App PINs` | WARN while an app's PIN is locked after repeated wrong PINs (lock reported within the last 15 minutes; dial-through, see [Apps](#apps)); wrong PINs of the last hour as metric | `pbx_app_pin_failures_1h`, `pbx_app_pins_locked` |
| `PBX SIP Logins` | failed logins at Asterisk in the last hour (wrong password, unknown account, refused by ACL; AMI security events): WARN from 10, CRIT from 100 or from 10 different sources; the text names the most frequent sources and accounts. Blocking (fail2ban, firewall) is the deployment's business, Asterisk's `security.log` has the details | `pbx_sip_auth_failures_1h`, `pbx_sip_auth_sources_1h` |

**Checkmk with the agent on the PBX host** (host networking, so 127.0.0.1 is allowed by default):
a local check that fetches the list, e.g. `/usr/lib/check_mk_agent/local/pbx`:

```sh
#!/bin/sh
curl -sk --max-time 20 https://127.0.0.1/monitor | tail -n +2
```

(`http://127.0.0.1:<port>` with `PBX_TLS=off`.)

**Without an agent:** every check also has its own URL, `/monitor/<key>`: `database`, `asterisk`,
`certificate`, `devices`, `calls`, `pins`, `sip-auth`, `trunk/<Asterisk name>` (e.g. `trunk/sip-1`, see the trunk
page). 200 = OK (or WARN), 503 = CRIT, 404 = unknown check; the body is the check's line. In Checkmk
one rule "Check HTTP web service" with one endpoint per URL gives one service each (expected status
200; the certificate check of the rule covers the expiry). The Checkmk server must be in
`PBX_MONITOR_NETS`. Disabled trunks answer 200 "disabled".

## Secrets (files, not variables)

Created by `patchbay-init` under `$PBX_DIR/secrets` (layout in [Deployment](#deployment)) and copied into the
containers by the start scripts (`*_FILE` variables point to them), nothing to configure: database
passwords, cookie and encryption keys, AMI/ARI passwords, `internal_key`, `oidc_client_secret` (to
be placed by hand, only with OIDC), `dns_api_token` (by hand, only for `acme`). Missing files are
created on every `docker compose up`, existing ones are kept; the per-service copies are rewritten
from them.

`internal_key` protects patchbay-web's internal webhooks (`127.0.0.1:8081`, voicemail intake, listening
by phone and the DND/forward feature codes): the host network is shared, so every call must carry it (HTTP Basic,
user `pbx`). Copies go to `secrets/patchbay-web/internal_key` (patchbay-web, `PBX_INTERNAL_KEY_FILE`) and
`secrets/asterisk/internal_key` (rendered into the dialplan global `PBX_INTERNAL_KEY` at Asterisk
container start). After changing it, `docker compose up -d --force-recreate asterisk patchbay-web`.

## Example

The smallest working env file has two lines, `PBX_HOSTNAME=voip.example.com` and
`PBX_COUNTRY_CODE=49`, plus `PBX_AUTH=local` for a first admin without single sign-on. Everything
else: [`deploy/pbx.env.example`](../deploy/pbx.env.example).
