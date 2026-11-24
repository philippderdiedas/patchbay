# Architecture

Patchbay is a self-hosted phone system for small organisations: a lean Asterisk 24 without a GUI
framework, a Rust web application (`patchbay-web`) for self-service, administration and device
provisioning, and PostgreSQL as the single source of truth. This document describes what the code
does today. Operation and every setting: [`configuration.md`](configuration.md); all variables:
[`deploy/pbx.env.example`](../deploy/pbx.env.example).

**Principles**

1. **PostgreSQL is authoritative** for persons, devices, numbers, groups, routes and contacts.
   Asterisk reads it through SQL views and `func_odbc`, so creating or locking a device needs no
   sync code and no reload.
2. **The repository holds only site-neutral code.** A site is an env file plus a state directory;
   everything site-specific is rendered at container start or managed in the web UI.
3. **patchbay-web is the only writer** of the business tables; Asterisk only reads (and writes CDR).
4. **Permission checks fail closed**: missing or ambiguous data means "deny", never "allow".
5. **Integrations hang off patchbay-web's business events**, not off raw Asterisk events.

Non-goals: call recording, audio in the browser, multi-tenancy (one instance per site, sites are
connected over IAX2), exposure to the internet (devices register from internal networks or a VPN).

## 1. Components

```
 browsers · Linphone (/prov) · CardDAV clients        OIDC provider · Keycloak admin API
                 │ HTTPS                                         │
                 ▼                                               ▼
        ┌─────────────────────────────── patchbay-web ──────────────────────────────┐ ◄─► Mattermost
        └───┬────────────────────────┬──────────────────────────────▲───────────────┘
        SQL │        AMI · ARI · generated files       internal webhooks (CURL)
            ▼                        ▼                              │
       PostgreSQL ◄───── ODBC ───── Asterisk 24 ────────────────────┘
                                     ├── SIP ──► provider trunks
                                     ├── IAX2 ─► other sites
                                     └── SIP (UDP/TCP/TLS) ◄── phones · DECT · ATAs · voice assistants
```

**Containers** ([`deploy/compose.yml`](../deploy/compose.yml), all with host networking):

| Service | Image | Role |
|---|---|---|
| `postgres` | `postgres:17` | database, listens on 127.0.0.1 only, `pg_hba.conf` written by `patchbay-init` |
| `patchbay-web` | `patchbay-web` | starts as root, runs `patchbay-init` (secrets, directories, interim certificate) and the migrations, then drops to UID 65534 |
| `asterisk` | `patchbay-asterisk` | Asterisk built from source; `site-config.sh` renders the site files, then starts Asterisk as user `asterisk`; waits until patchbay-web is healthy |
| `acme` (profile) | `acme.sh` | optional Let's Encrypt via DNS-01 into `keys/` |
| `backup` (profile) | `postgres:17` | optional daily `pg_dump` |

All state lives in one directory (`$PBX_DIR`, default `./pbx`) plus the PostgreSQL data directory.

**Interfaces between patchbay-web and Asterisk**

| Channel | Direction | Used for |
|---|---|---|
| ODBC (role `asterisk`) | Asterisk → PostgreSQL | PJSIP realtime (devices), `func_odbc` lookups (numbers, targets, routes, permissions), MOH and queue realtime, CDR insert |
| AMI (user `pbxweb`, 127.0.0.1) | both | events for call tracking and failed SIP logins; actions: originate (click-to-call, ring test, pickup), hangup, blind transfer, mute, astdb, sorcery cache expiry, reloads, status queries |
| ARI | patchbay-web → Asterisk | MWI state only (`res_mwi_external`) |
| Internal webhooks (`127.0.0.1:8081`) | Asterisk → patchbay-web | `CURL` from the dialplan: new voicemail, voicemail list/actions for `*97`, DND and forward feature codes, script apps, wrong app PINs |
| Generated files (`$PBX_DIR/generated`) | patchbay-web → Asterisk | `pjsip_trunks.conf`, `iax_peers.conf` (trunks), `hints.conf` (busy lamps), `extensions_custom.conf` (custom dialplan apps) |
| Rendered at start (`site-config.sh`) | env → Asterisk | ACLs, transports, globals (country code, emergency numbers and trunk, internal key), ODBC/AMI/ARI credentials |

The AMI user gets only the classes patchbay-web needs. After writing trunk files patchbay-web
reloads, checks every object and restores the previous file if one is missing. Asterisk caches
devices in memory (stale after 60 s), so registrations and emergency calls survive short database
outages; patchbay-web expires a changed device from the cache and empties the caches at start.

## 2. Number plan and call routing

All phones send the number en bloc. **Every internal number has exactly 4 digits (1000–9999) and
exists once** in the registry table `numbers`, kept in sync by triggers for persons, devices, call
groups, IVRs and apps. The registry, not the length, says what a number is
(`ODBC_NUMBER_KIND`). Ranges and allocation (`next`, `random`, `choose`) per kind come from
`PBX_NUMBERS_*`; released numbers stay blocked for a cool-down.

| Dialled | Meaning |
|---|---|
| `1000`–`9999` | internal number (person, device, group, IVR, app) |
| `<prefix>` + ≥ 4 digits | another site over IAX2, via the dial rules |
| 2–3 digits, `116xxx` | emergency and service numbers, via the dial rules |
| `0…`, `+…` | outside, via the dial rules |
| `*…`, `#…` | feature codes |

| Feature code | Function |
|---|---|
| `*8` / `*8<ext>` | pick up a ringing call in the own pickup group / at a colleague (same pickup group) |
| `*761` / `*760` | do not disturb on / off (the person, all devices) |
| `*21*<number>` / `#21#` | immediate forward on / off (target checked against the person's own permissions) |
| `*97` | listen to own and call group voicemail (no PIN, identified by the device) |
| `*43` | echo test |

Feature codes work only from an own device (the caller is identified by its endpoint). Transfers
are SIP REFER from the phone; in-call DTMF transfer is off (`Dial()` never gets `t`/`T`), so an
external caller can never transfer a call out by key presses.

### Dialplan contexts (`asterisk/etc/extensions.conf`)

| Context | Purpose |
|---|---|
| `from-device` | calls from own devices: feature codes → internal numbers (`sub-internal`) → everything else to `sub-outbound` |
| `sub-internal` | resolve an internal number by kind, restricted to the kinds the caller may reach |
| `sub-user` | call a person: personal blocklist, forward, DND, devices, no-answer target |
| `group-member` | ring only a person's devices (no forward, no voicemail); used by groups, queues, recall, click-to-call |
| `sub-group` | call group: `ringall`, `linear`, `rrmemory` dial loop, or queue mode |
| `sub-target` | execute a target (person, group, IVR, mailbox, announcement, external number, app, hang up) |
| `sub-outbound` | dial rules with permission check |
| `from-sip-trunk` | inbound SIP: DID mode (DID → time profile → target) or hand over to `trunk-in` |
| `from-iax`, `trunk-in` | inbound IAX2 and passthrough SIP: trunk inbound rules |
| `sub-ivr`, `sub-app`, `sub-mailbox`, `vm-listen` | voice menus, apps, voicemail recording, `*97` menu |
| `sub-recall`, `ring-delay`, `pbx-xheaders` | transfer recall, delayed ringing, call context headers |
| `click2call`, `ring-test`, `hints` | originated by patchbay-web; generated busy-lamp hints |

`flow.rs` (call flow diagrams) and `routetest.rs` (route tester) mirror these contexts; keep them in sync.

### Calling a person and a group

`sub-user`: a caller on the person's personal blocklist hears busy; an immediate forward goes to
its target (at most 3 hops), DND to the no-answer target. Otherwise `Dial()` rings all devices with
ringing on, in `ring_order`, optionally delayed per device (mobile phones as `Local/<delay>-<id>@ring-mobile`,
dialled out by `sub-outbound` with the person's permissions and `PBX_PARALLEL`, which makes a refused leg
end silently instead of answering with an announcement), with option `c` ("answered elsewhere"
instead of "missed"), showing the caller name from the person's own contacts. No answer → no-answer
target (mailbox, group, person, external number) or "not available".

Call groups do not use `Queue()` by default but a dial loop over `Local/<ext>@group-member`, so
members ring only their devices and never their forwards. The member order (Admin → call group,
drag & drop) is the ringing order for "one after another" and the start order for round robin. Options: announcement before ringing,
hold music instead of ringback, a caller-name prefix shown only on members' phones, member and
total timeouts, then the group's no-answer target. A group can switch to **queue mode**
(`app_queue` with the realtime views `queue_rt`/`queue_members_rt`): only free members ring (busy
state from the person's hint), waiting callers hear their position; members log in and out per group.

**Transfer recall:** an unanswered blind transfer to a person or group rings the transferring person
once more (`PBX_TRANSFER_RECALL_S`) before the target's no-answer rule applies.

### Outbound: route classes and dial rules

`sub-outbound` normalises the number (`00`→`+`, `+<own cc>`→`0…`, other `+`→`00…`, `0112`→`112`),
then determines **whose permissions apply**, in this order:

1. **Blind transfer** (`BLINDTRANSFER`): the transferring device's person – never the trunk the
   transferred call came in on (otherwise an external caller could dial out with trunk rights).
2. **Click-to-call** (`PBX_C2C_EXT`): the person who clicked.
3. **Own device**: the device's person.
4. **Call from a trunk** (forward, transit): the trunk's classes (provider trunks have only `internal`).
5. A **self-service forward** to an external number is checked with the forwarding person's classes
   on every call, not only when it was saved. An external target set by an admin (forward, DID or
   IVR target) is allowed once for exactly that number (`PBX_ALLOW_EXTERNAL`).

The first matching **dial rule** (`outbound_routes`: priority, prefix, length; SQL `route_for`)
gives `deny` or `route` with up to five trunks in order (`route_trunks`), digits to strip/prepend,
caller-ID mode (person's DID or the rule's) and a **route class**. The call goes out only if the
class is among the caller's effective classes (`user_effective_classes` = own classes ∪ classes
granted through an identity provider group). Empty classes deny. Each trunk has a channel limit
against toll fraud.

**Trunk selection by the caller's number.** SQL `route_trunk_order(rule, caller's outbound number)`
gives the trunks one call tries and the caller ID for each (func_odbc `ROUTE` encodes them as
`ref/kind/max/clip/clip trunk|…`; the route tester uses the same function):

- Order: the rule's switched-on trunks in their order; the trunk that owns the caller's outbound
  number (`users.outbound_did_id` → `dids.trunk_id`) moves to the front if the rule lists it and
  shows the person's number (caller-ID mode `user`). Emergency rules keep their order. Callers
  without an outbound number get the rule's order.
- Caller ID: mode `user` = the person's number, else the rule's; `route` = the rule's. On a trunk
  marked **shows only its own numbers** (`trunks.clip_own_only`, a provider that replaces any
  caller ID with the account's number) a number of another trunk is not sent: the rule's number if
  it belongs to that trunk, else none (the extension goes out, the provider shows its own). Other
  trunks behave as before. As before only numbers of the own country are set (as `0…`).
- `sub-outbound` tries the trunks in turn, each with its own checks: an IAX2 peer the call came in
  on is skipped, the channel limit (`GROUP`) skips a full trunk, the caller ID is set per trunk.
  **A callee never gets a second call:** the next trunk is tried only after a full trunk or a
  `Dial` that ended `CHANUNAVAIL`/`CONGESTION` *before any ringing or progress* (`RINGTIME` and
  `PROGRESSTIME` empty; Asterisk maps many final SIP answers, even after ringing, to
  `CHANUNAVAIL`) and not for a reason of the number or callee: the Asterisk cause of the dialled
  channel (`HANGUPCAUSE`, cleared before every attempt) in `PBX_FINAL_CAUSES` (1 unallocated
  404/604, 17 busy 600, 19 480, 20, 21 rejected 403/603, 22 410, 28 484) ends the call. Busy, no
  answer, answer and cancel always end it. Emergency calls ignore `PBX_FINAL_CAUSES` (a 403/404/484
  often comes from the trunk itself: blocked account, refused caller ID, no emergency routing), only
  ringing or progress ends them. All trunks full → busy; no usable trunk → denied.

| Example rule | Action |
|---|---|
| `110`, `112`, `116` + 3 | route to the provider, class `emergency` |
| `<site prefix>` + ≥ 4 | route to the IAX2 peer, strip the prefix |
| `0900`, `0137`, `118`, … | deny |
| `00` | route to the provider, class `international` |
| `0` | route to the provider, class `national`, caller ID of the person |
| `0` (several accounts) | trunks `[B, A]`: persons whose number belongs to `A` go out over `A`, everyone else over `B`; each falls back to the other when it cannot take the call |

### Inbound

| Trunk mode | Flow |
|---|---|
| SIP, **DID** | admin blocklist → busy. Called number (request URI/To) → E.164 → `dids`; unknown → the trunk's own number. Time profile (`PBX_TIMEZONE`): holiday/closing day → holiday target (else closed), otherwise open or closed target. `PBX_DID` stays on the channel for call tracking, voicemail and the `X-PBX-DID` header. |
| SIP, **passthrough** | for accounts where every extension arrives on one trunk: admin blocklist → busy, then the called number (leading zeros stripped) goes through the trunk inbound rules like IAX2 |
| IAX2 | loop check (below), then trunk inbound rules; no admin blocklist (callers are extensions of other sites) |

**Trunk inbound rules** (`trunk_inbound_rules`, by priority, then longest prefix): `internal` (with
the kinds of numbers that may be reached, e.g. persons and groups only), `target` (a fixed target,
e.g. an app), `outbound` (transit through our dial rules with the trunk's classes) or `reject`.
These trunks also have an inbound channel limit. Alias apps keep the rule's allowed kinds, so an
alias cannot widen what a trunk rule allows.

### IAX2 network

- **A site has no prefix of its own.** Each IAX2 trunk carries the prefix under which *our* users
  reach that peer; how the peer reaches us is the peer's business, as in the public network.
- Prefixes are 2–4 digits, never start with `0` (dialling out) or `11` (emergency/service numbers),
  and the set of prefixes must be prefix-free; patchbay-web rejects violations.
- Outbound: `<prefix>` + ≥ 4 digits → the peer, prefix stripped, caller ID = plain extension. Inbound
  caller IDs of ≤ 4 digits get the trunk's prefix in front, so the callee can call back.
- Logins: our name at the peer is set per trunk; the peer logs in here under the trunk's Asterisk
  name. MD5 with a shared secret, call tokens, a source ACL per peer; AES encryption per trunk
  (optional, as links usually run over a site VPN).
- **Loop protection:** the IAX variable `PBX_PATH` carries the site ids (`PBX_SIP_REALM`) the call
  passed; a site that sees itself again, or a path of 4 sites, rejects the call. An IAX2 call is
  never routed back out over the peer it came in on.

## 3. Data model

The schema is `db/migrations/0001_initial.sql`; later changes come as numbered files, applied by
`patchbay-web migrate` at every start (transaction per file, advisory lock, `schema_migrations`):
`0002_route_trunks.sql` (trunk list per dial rule, `trunks.clip_own_only`).

| Area | Tables |
|---|---|
| Persons and devices | `users` (`kind` person/function, extension, permissions, DND, forward, ring time, pickup group, languages), `devices` (one SIP account each), `local_logins`, `provisioning_tokens` |
| Numbers | `numbers` (registry), `number_holds` (cool-down), `number_policy` |
| Routing | `targets` (every "target" column points here), `groups`, `group_members`, `ivrs`, `ivr_options`, `time_profiles`, `time_rules`, `profile_closures`, `holidays`, `holiday_sources`, `trunks`, `dids`, `inbound_rules`, `trunk_inbound_rules`, `outbound_routes`, `route_trunks` (a rule's trunks in order), `route_classes`, `blocked_numbers` |
| Apps and media | `apps`, `app_media`, `app_members`, `custom_apps`, `sounds`, `moh_playlists`, `moh_media`, `greetings` |
| Personal data | `contacts`, `dav_tokens`, `voicemails`, `call_notes`, `user_blocked_numbers`, `bot_*` |
| Operation | `cdr`, `audit_log` (every write by patchbay-web), `settings` (site values the dialplan reads) |

Users, groups and devices are soft-deleted so CDR, voicemail and audit keep their references; a
person or app that is still a call target cannot be deleted (the message names the places).

**Locking has two levels:** `devices.enabled` (user or admin) and `users.phone_allowed` (identity
sync or admin). The views filter on both, so when a person gets the permission back exactly the
devices that were active before come back.

**PJSIP realtime.** Device identity = a random `sip_username`: AOR, auth user and SIP identity are
the same, so the registrar needs no `identify_by` tricks; name and extension shown to others come
from `callerid`. Views (devices only; trunks are generated files):

| View | Content |
|---|---|
| `active_devices` | enabled, not deleted devices of enabled persons with `phone_allowed` |
| `ps_endpoints` | transport, codec order per device type, caller ID `"<name>" <ext>`, SRTP, ACL per transport, mailbox for MWI, pickup groups, hold music class |
| `ps_auths` | `digest` with `password_digest = md5:<ha1>`, realm from `settings` |
| `ps_aors` | one contact, qualify every 60 s; a fixed address becomes a static contact without qualify |

Other views: `user_effective_classes`, `group_ringing_members` (logged-in members with a ringing
device), `queue_rt`, `queue_members_rt`, `musiconhold`, `app_dialplan`, `app_group`. SQL functions
used by the dialplan: `route_for`, `route_trunk_order`, `may_dial`, `time_profile_state`, `caller_blocked`,
`user_caller_blocked`, `contact_name`, `caller_e164`.

**Database roles (least privilege)**

| Role | Used by | Rights |
|---|---|---|
| `pbx` | migrations | owner: DDL, creates the other roles, sets their passwords from secret files |
| `pbxweb` | patchbay-web | data only (SELECT/INSERT/UPDATE/DELETE), no DDL |
| `asterisk` | Asterisk (ODBC) | SELECT on the views and the routing tables it needs (incl. `route_trunks`; `route_for`/`route_trunk_order` run with its rights), column-level on `users` and `trunks` (never `secret_enc`), INSERT on `cdr`; no access to contacts, notes, voicemails, logins, bot or audit tables – personal data reaches the dialplan only through `SECURITY DEFINER` functions that return one value (`contact_name`, `user_caller_blocked`) |

## 4. Security model

| Topic | Design |
|---|---|
| Permission checks | Fail closed everywhere: an ODBC lookup that returns nothing denies (an empty answer from an ambiguous query must never look like "allowed"). Rule for new checks: test the "empty/missing data" case. |
| Blind transfers | Checked with the transferring person's classes, never the trunk's; same for transfers from the web widget (AMI `BlindTransfer` sets `BLINDTRANSFER` like a REFER). |
| Forwards | Self-service forwards to external numbers are re-checked on every call; admin targets are a one-shot permission for exactly that number. |
| Emergency calls | Always allowed from own devices, no channel limit, leading `0` corrected, the rule's trunk order (no preference by the caller's number). If the database gives no rule, or no trunk of the rule takes the call, the dialplan dials `PBX_EMERGENCY_TRUNK` (unless already tried). **Control-centre callback:** the dialplan remembers trunk and caller (astdb `pbx_sos/<trunk>`) for the trunk actually used and for the trunk that owns the number shown (the control centre calls that number back); an attempt that does not take the call puts the previous records back, unless a concurrent emergency call has written its own since; for 30 minutes any call arriving on that trunk in DID mode (and not on the admin blocklist) first rings the emergency caller's devices for 25 s (past DND and forwards, caller name "SOS …") – the control centre calls back from numbers we cannot know. |
| PIN brute force | App PINs (dial-through, conference) are counted per app across calls in the astdb: the 5th wrong dial-through PIN in 15 minutes locks the app for 15 minutes (every PIN is then wrong, no hint to the caller). Conference rooms are never locked (anyone could lock colleagues out). Failures are logged, audited and exposed in monitoring; counting works without patchbay-web. Local web logins: 5 failures lock the login name (growing), 20 per IP lock the IP; unknown names cost the same time. |
| Script apps | Rhai without modules or `eval`, limits on operations, sizes, call depth and 10 s run time; the only way out is `http_get` (GET, 3 per run, size and time limits, no redirects), with the address checked after DNS resolution and connected to directly; internal networks only when listed in `PBX_SCRIPT_HTTP_ALLOW`. Script apps are never part of the default user app kinds. |
| Custom dialplan apps | Admins only; the `strict` guard (allowlist of applications, functions, jump targets) catches mistakes but is not a security boundary. |
| Web UI | Encrypted session cookie (`Secure`, `HttpOnly`, `SameSite=Lax`). **CSRF:** every non-GET request must carry `Origin` = `PBX_PUBLIC_URL` (or no `Origin` and `Sec-Fetch-Site` same-origin); SameSite alone would trust sibling hosts of the same domain. Absolute URLs are built from `PBX_PUBLIC_URL`, never from `Host`/`X-Forwarded-*`. Chat callbacks (`/bot/…`) are outside the origin check and authenticated by token or HMAC signature. |
| Internal webhooks | Loopback listener, HTTP Basic with `internal_key`, compared in constant time – host networking means every local process could otherwise reach it. |
| Secrets | Files, never variables; created by `patchbay-init` (kernel CSPRNG), each container gets only its own copies. Device passwords: only `ha1` is stored; manual SIP devices see the plain password once. Trunk passwords: AES-256-GCM in the database, key in `secret_key` on the host (a database dump alone does not reveal them). Provisioning tokens and chat link codes are stored as SHA-256 hashes; local passwords as argon2id. |
| SIP | Devices register only from `PBX_DEVICE_NETS_UDP`/`_TLS` (ACL per transport); no anonymous identifier. SRTP mandatory over TLS; over UDP/TCP only "SRTP if possible" (SDES keys would travel in clear). TLS 1.2/1.3. |
| Blocklists | Admin blocklist (numbers or prefixes, anonymous callers; checked first on every SIP trunk, DID and passthrough mode, not on IAX2 links) answers busy without a hint; personal blocklist per person (below). |
| Monitoring | `/monitor` (Checkmk local check format, per-check URLs with 200/503) and `/metrics` (Prometheus) answer only clients in `PBX_MONITOR_NETS`, others get 404. Checks: database, Asterisk, each trunk, certificate expiry, devices, active channels, locked app PINs, failed SIP logins per hour (from AMI security events; blocking is left to the deployment). |
| Privacy | No recordings. Personal contacts, notes and blocklists are visible only to their owner; logs name no caller numbers in companion actions. |

## 5. Devices and provisioning

| Type | Setup | Codec order |
|---|---|---|
| Linphone | self-service or admin; QR code, TLS + mandatory SRTP | Opus, G.722, A-law |
| SIP device (manual) | UDP/TCP/TLS, credentials shown once; DECT bases, desk phones, ATAs, other PBXs connecting as phones | G.722, A-law, Opus |
| SIP device with a fixed address | admin; `contact_uri` instead of registration, no credentials | G.722, A-law, Opus |

The PBX decides the codec (`prefer: configured`, `keep: first`), so two G.722 phones never
transcode. Video (H.264, VP8) is allowed for devices. **Opus** comes from the open-source
transcoder `codec_opus_open_source` (traud/asterisk-opus, fetched and checked in `asterisk/Dockerfile`)
(Sangoma's binary module may not be redistributed). Prompts and hold music are 16 kHz, so Opus
gains nothing over G.722 for them. Trunks offer G.722, A-law, µ-law and Opus per trunk.

**Linphone provisioning:** "Add device" creates a device with a number from the device range, a
random `sip_username` and a random password used only to compute `ha1`. The QR code holds
`<PBX_PUBLIC_URL>/prov/b/<bootstrap token>` (single use, 15 minutes); the first fetch issues a
permanent device token (stored as hash) as `config-uri`, so the app refreshes its configuration at
every start. The XML (`lpconfig`, every entry `overwrite="true"`) sets identity and TLS proxy,
`ha1` auth, mandatory SRTP, the codec list, `dial_prefix` = `PBX_COUNTRY_CODE` (numbers are sent
unchanged, the PBX normalises them), MWI and a CardDAV remote contact directory (`/carddav/book/`,
Linphone searches it with its SIP credentials). Locking or deleting a device removes it from the
views, revokes the tokens, expires the sorcery cache and hangs up its calls.

**Admin-managed devices** are read-only for the person (except ringing and ring delay). **Devices
with a fixed address** (e.g. Home Assistant's VoIP integration) are called directly and never
qualified: such endpoints often ignore OPTIONS and would otherwise be marked unavailable.

**Call context for voice assistants:** every call to a device carries `X-PBX-Caller`,
`X-PBX-Caller-Name` (as shown on phones, incl. phone book names and group prefixes) and `X-PBX-DID`
(the own number the call came in on; missing for internal calls). Phones ignore them; voice
assistants connect as a device with a fixed address, or as an IP trunk when they also call out.

## 6. Voicemail, media and phone books

- **Voicemail without `app_voicemail`:** `sub-mailbox` plays the own greeting and records; a hangup
  handler reports the file to patchbay-web, which converts it to MP3, notifies by mail and/or chat
  (personal choice), publishes `voicemail.new` and sets MWI over ARI. Messages are kept 90 days.
  `*97` lists own and group messages through the internal webhook; heard/delete go through the same
  function as the web UI (access check, audit, MWI), never as database writes from Asterisk.
- **Audio uploads** are converted once on upload (ffprobe check, loudness-normalised ffmpeg) to
  `.sln16` and `.alaw` plus a browser preview, so Asterisk plays natively without CPU per call.
  Hold music classes are realtime (`musiconhold` view): company playlists (one is the default),
  personal playlists (`PBX_USER_MOH`) and per-app classes.
- **Phone books (CardDAV):** colleagues (read-only), personal contacts (read-write with an app
  password), services (emergency numbers, feature codes without arguments, groups, IVRs, company
  apps) and a combined book for Linphone. Personal contacts also name callers on the owner's phones
  (`contact_name`, per dial only).

## 7. Apps

An app is a **device without hardware**: it has an internal number and is reachable internally,
over the IAX2 network and as a call target (`targets.kind = 'app'`), i.e. from a DID, a trunk
inbound rule, a forward or a no-answer rule. The dialplan branch is `sub-app`; settings are
`apps.config` (validated by patchbay-web, read field by field through the `app_dialplan` view).

| Kind | On a call |
|---|---|
| Conference | ConfBridge room; optional room PIN and moderator PIN (`*0` in the room), max. participants, hold music while alone; keys `*1` mute, `*2` lock, `*3` kick last, `*4` mute others |
| Music | plays a playlist in a loop (own uploads, fixed or shuffled) |
| Announcement | plays a recording n times and hangs up |
| Echo | echo test |
| Speaking clock | "at the tone it is …" every 10 s, tone on the 10-second mark, ends after 10 minutes |
| Call group | members (added by extension, may leave themselves) ring at once via `group-member`; optional caller-name prefix |
| Script | a Rhai script produces text spoken through a Wyoming TTS server (`PBX_TTS`); test run on the app page |
| Alias | forwards to another internal number (internal only, at most 3 hops) |
| Dial-through (`gateway`) | PIN, then a number ending with `#`; dials fixed prefix + number through `sub-outbound` with a one-shot permission for exactly that number |

**Organization and personal apps.** Admins create organization apps (number range
`PBX_NUMBERS_COMPANY`, all kinds). Persons with phone permission create personal apps
(`PBX_NUMBERS_APP`) when `PBX_USER_APPS` allows the kind. Apps inherit reachability, not their
owner's rights: no app can dial out, except the dial-through, which is therefore organization-only
(`apps::COMPANY_ONLY_KINDS`; `app_dialplan` never activates a personal one) – whoever knows its PIN
dials anything behind its prefix, so the prefix should be narrow (e.g. a site prefix). Only
organization apps can be call targets, because a personal app could be changed or deleted by its
owner at any time.

**Custom dialplan apps** (admins): MiniJinja templates (data: users, groups, IVRs, sounds, site)
rendered into `[app-custom-<id>]` contexts in a generated file, re-rendered every minute; reached
through a number from the organization range.

## 8. Call companion (web widget)

A widget on every page shows the logged-in person's calls and lets them act from the browser. Phones
stay the audio devices; the companion never carries audio. It is **stateless on purpose**: every
poll (`/me/live`, every few seconds) reads the live channel list from Asterisk (`CoreShowChannels`,
cached for one second) and maps channels to devices and persons, so the widget cannot drift from
what Asterisk does; only "via" (DID, group) comes from call tracking.

| Feature | How |
|---|---|
| Live calls | ringing, active and outgoing calls of all own devices, with caller name, "via", devices, duration |
| Decline | hangs up the own ringing legs; other group members keep ringing |
| Reject (busy) | personal calls only: astdb marker `pbx_divert/<linkedid>-<ext>`, then hang up; `sub-user` answers busy instead of the no-answer rule (a transferred call first goes back to the transferring person) |
| Hang up, mute, blind transfer | AMI `Hangup`, `MuteAudio`, `BlindTransfer` into `from-device` (same permission checks and recall as a transfer from the phone) |
| Pickup button | colleagues' ringing calls in a shared pickup group; rings the own devices, then `PickupChan` |
| Toggles | DND and group log in/out; browser notifications on incoming calls (opt-in) |
| Dial field | click-to-call with suggestions from the phone book (colleagues, groups, services, own apps, contacts) |
| Caller context | last own calls with the number, voicemails from it, "save as contact" / "open contact" |
| Personal blocklist | "block this number for me" (full E.164 numbers, max. 200, hit counter, managed on the start page); checked in `sub-user` before `PbxTarget`, so a blocked call is no missed call; group and queue calls still ring |
| Call notes | per external number, shown on calls from/to it; personal by default, "visible to all colleagues" only with `PBX_SHARED_NOTES=on`; max. 1000 characters, 500 per person |

Every action re-reads the channels and only touches channels of the person's own devices; actions
are audited (`call.*`, `user.block`, `note.*`) without caller numbers in the log.

## 9. Call tracking, events and chat bot

**Call tracking** (`calltrack.rs`) uses AMI rather than ARI: AMI is needed anyway and delivers every
channel event without a Stasis app that would own the calls. One permanent connection (event mask
`call,user,security`) with reconnect and reconciliation via `CoreShowChannels`. A call is all
channels with the same `Linkedid`; the dialplan marks business steps with `UserEvent`s (`PbxTarget`,
`PbxNoAnswer`, `PbxVoicemail`, `PbxRecall`). The tracker is a pure state machine tested with
recorded event sequences.

Every finished call that reached a target or dialled somewhere is also stored as a **call flow**
(`call_flows`, one JSON row per `Linkedid`): origin (device, trunk or IAX peer and the number dialled),
DID, targets with their outcome, every `Dial` leg (endpoint, number on a trunk, caller ID sent,
`DialStatus`), voicemail and who answered. The admin call log (`callflow.rs`) shows one line per call,
e.g. `1020 Tim · Gigaset (2004) → via 053 (shows 040…) → 0176…`; calls without a stored flow (older,
or patchbay-web was down) are reconstructed from their CDR rows, without IVR and group steps.
Retention follows `PBX_RETENTION_CALLS_DAYS`.

The **event bus** (`events.rs`) adds names from the database and broadcasts `call.incoming`,
`call.answered`, `call.missed`, `call.ended`, `voicemail.new` and `presence.changed` (from the
persons' hints and DND). Publishing never blocks; slow consumers lose events.

**Chat bot** (crate `patchbay-notify`, compiled into patchbay-web): platform-neutral message model
and bot logic behind the trait `ChatPlatform`; Mattermost is the implemented adapter (REST v4 +
WebSocket, slash command, buttons signed with HMAC-SHA256). The bot is part of patchbay-web rather
than a generic SIP bot because the meaning (persons, groups, voicemail, permissions) lives there.

- Call groups linked to a channel (`/pbx link-group <number>`, PBX admins only; never automatic) get
  a post per incoming call, edited on answer or miss.
- Persons get direct messages for missed calls and, per personal setting, voice messages as MP3
  with "heard / call back / delete" (same function as the web UI).
- Commands: `help`, `status`, `dnd`, `forward`, `noanswer`, `ringtime`, `call`, `mailbox`, `pb`,
  `who`, `calls`, `link`, `unlink`, `groups`; calls use click-to-call, so the dialplan checks
  permissions.
- Linking chat ↔ PBX user: `link` (one-time code from the *Chat* page, 15 minutes) or additionally
  `auto` (unique e-mail address, where the platform reveals it).

## 10. Identity and login

`PBX_AUTH=oidc|local|both`. OIDC uses authorization code + PKCE with groups in the `groups` claim
(`PBX_GROUP_ADMIN`, `PBX_GROUP_PHONE`, `PBX_GROUP_WEBUI`). A person is linked to an account by its
`sub`, explicitly by an admin or by provisioning, never by e-mail address.

- **Permissions:** per-user switches *Web interface*, *Administrator* and *Phone access*
  (`phone_allowed`). Local logins use only the switches; OIDC logins get access through the groups
  **or** the switches. Phone access applies to the person however they log in.
- **Keycloak sync** (`PBX_OIDC_SYNC=keycloak`, every 10 minutes, service account): refreshes groups,
  and without the phone group or with a disabled account sets `phone_allowed = false`, which locks
  all devices. Group membership in the token applies to the web session; for telephony the sync
  decides. `off` works with any OIDC provider (the admin enters the `sub`).
- **Provisioning modes** (`PBX_PROVISION`): `admin` (admins create persons), `auto` (every member of
  the phone group gets a person with the next free number at login or in the sync), `self` (users
  claim a number on the start page; phone permission without the phone group).
- **Function users** (`kind = function`, e.g. a reception phone) have no identity provider account
  and are managed by admins.
- **Local logins** (`local_logins`): login name + argon2id hash; session subject `local:<user id>`
  with a password version, so a new password ends all other sessions. The first admin is created
  with `patchbay-web set-password … --create … --admin`.
- Numbers, ring behaviour and forwards are never stored in the identity provider.

## 11. Housekeeping and retention

| Job | Interval | What |
|---|---|---|
| Voicemail retention | daily | messages older than 90 days, with their files |
| CDR / audit log retention | daily | `PBX_RETENTION_CALLS_DAYS`, `PBX_RETENTION_AUDIT_DAYS` (0 = keep) |
| TTS cache | daily | spoken texts unused for 30 days |
| Asterisk logs | daily | `logger rotate`, seven generations |
| Keycloak sync | 10 minutes | groups and phone permission |
| Custom apps | 1 minute | re-render templates |
| Busy-lamp hints | on change | rewrite `hints.conf`, reload dialplan and `res_pjsip` |
| Holiday sources | daily | re-import sources marked "keep updated" |
| Certificate watch | 5 minutes | restart Asterisk when no call is active (`core restart when convenient`, forced after `PBX_ASTERISK_RESTART_MAX_WAIT`), because the PJSIP TLS transport loads its certificate only at start |
| Database backup | daily (optional service) | `pg_dump -Fc`, `PBX_BACKUP_KEEP` dumps |

