<img src="docs/logo.svg" alt="" width="120" align="right">

# Patchbay

A self-hosted phone system for small organisations: Asterisk 24, a Rust web application
(`patchbay-web`) for self-service, administration and device provisioning, and PostgreSQL as the
single source of truth. No GUI framework, no site-specific code: a site is an env file plus a state
directory the containers create themselves.

## Features

- Persons with one extension and any number of devices: Linphone provisioned by QR code, manual SIP
  devices (DECT, desk phones, ATAs), devices with a fixed address (e.g. Home Assistant VoIP, voice
  assistants with `X-PBX-*` call context headers).
- Call groups (ring all, linear, round robin, queue mode), IVRs, time profiles with holiday
  calendars, transfer recall, pickup, DND and forwards (web, feature codes, chat).
- SIP trunks (DID or passthrough inbound) and IAX2 links to other sites, managed in the web UI;
  dial rules with permission classes, fail-closed checks, emergency fallback trunk.
- Digital voicemail (web, mail, chat, `*97`), MWI, busy lamps.
- Apps: conference, music, announcement, echo, speaking clock, call group, alias, script (Rhai +
  text to speech), dial-through; custom dialplan apps for admins.
- Call widget in the browser: live calls, decline/reject, transfer, pickup, dial field, caller
  context, call notes, personal blocklist.
- Phone books over CardDAV (colleagues, personal contacts, services); caller names from contacts.
- Login via OIDC (Keycloak group sync, auto/self provisioning) and/or local accounts.
- Chat bot for Mattermost: missed calls, voicemail, group channels, commands.
- Monitoring endpoints for Checkmk and Prometheus.
- Codecs: G.722, G.711, Opus (open-source transcoder traud/asterisk-opus, built into the Asterisk image).

## Quick start

You need the two images, [`deploy/compose.yml`](deploy/compose.yml) and an env file from
[`deploy/pbx.env.example`](deploy/pbx.env.example). The images are built by the CI into this
project's container registry (`patchbay-web`, `patchbay-asterisk`; tags: commit SHA, `latest`,
release tags `vX.Y.Z`): set `PBX_IMAGE_PREFIX` to the registry path of this project with a trailing
slash and `PBX_VERSION` to a tag. `deploy/build.sh` builds them locally instead.

```sh
mkdir patchbay && cd patchbay
cp compose.yml docker-compose.yml
cp pbx.env.example .env
$EDITOR .env                    # required: PBX_HOSTNAME, PBX_COUNTRY_CODE; PBX_IMAGE_PREFIX/PBX_VERSION;
                                # e.g. PBX_AUTH=local to start
docker compose up -d
# first admin (local login), the password is read from stdin:
docker compose exec -T patchbay-web patchbay-web set-password auto --create "Jane Admin" --admin < password.txt
```

Certificates, reverse proxy, backups, chat bot and monitoring:
[docs/configuration.md](docs/configuration.md).

## Documentation

- [docs/configuration.md](docs/configuration.md) – deployment, operation and configuration
- [docs/architecture.md](docs/architecture.md) – how it works: components, call routing, data
  model, security model
- [deploy/pbx.env.example](deploy/pbx.env.example) – every variable with default and description

License: GPL-3.0-or-later (see LICENSE). The Asterisk image builds the open-source Opus transcoder from traud/asterisk-opus (GPL-2.0, like Asterisk itself), fetched at build time.
