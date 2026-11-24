# Webhooks

HTTP requests to other systems (CRM, ERP, chat, automation) on events of the phone system. Patchbay
knows no target system: every request is built from templates in a hook file, so almost any API can
be called. Hooks are static files, versioned with the site's configuration.

## Files

`$PBX_DIR/config/webhooks.d/*.toml` (in the container `/config/webhooks.d`, other directory:
`PBX_WEBHOOKS_DIR`), read when patchbay-web starts; changes need a restart. One file can hold several
`[[webhook]]` tables; a broken file or hook is skipped and shown on **Admin → Webhooks**, the others
keep working.

```toml
[[webhook]]
name = "crm-calllog"                 # unique: letters, digits, - _ .
events = ["call.ended"]              # see "Events"
when = "call.external and contact"   # optional: expression on the template data
method = "POST"                      # GET, POST, PUT, PATCH, DELETE (default POST)
url = "https://crm.example.org/api/calls"
headers = { Authorization = "Bearer {{ secret('crm_token') }}", "Content-Type" = "application/json" }
body = '''{ "phone": {{ remote.e164 | tojson }}, "seconds": {{ call.talk_s }} }'''
private_notes = false                # also notes only their author sees (default: shared notes only)
timeout_s = 15
max_attempts = 12                    # retries after errors (see "Delivery")
enabled = true
```

**Templates** are [minijinja](https://docs.rs/minijinja) (Jinja syntax: `{{ … }}`, `{% if %}`,
filters like `tojson`, `urlencode`, `join`, `map(attribute=…)`, `replace`, `default`). Missing values
render empty, so optional parts work with `{% if contact %}…{% endif %}`.

**Values are data from outside** (caller names, notes, fields of an ERP): never put them into a
request unquoted. Inside JSON every text value goes through `| tojson`, every number through `| int`
(`"org_id": {{ contact.x.some_id | int }}`; anything else than a number stops the request: the
delivery fails and nothing is sent); in a URL every value through
`| urlencode`, and never build the host from data. Values are never evaluated as templates themselves.

**Secrets**: `{{ secret('name') }}` reads the file `$PBX_DIR/secrets/patchbay-web/<name>` (in the
container `/run/patchbay-web/<name>`, copied at start). Tokens never go into the hook files.

**Delivery**: every matching event is queued (`webhook_deliveries`) with its data; a background loop
renders the request from the hook as it is at that time and sends it (several at once, so one slow
target does not hold up the others). 2xx = done. Network errors, timeouts, 408, 429 and 5xx are
retried until `max_attempts` (30 s, 1 min, 2 min … at most 6 h apart); other answers (e.g. 400, 401),
template errors and switched-off hooks fail at once. No redirects are followed. Secret values are
masked (`***`) in stored errors and answers. **Admin → Webhooks** lists the hooks,
loading errors and the last 200 deliveries (status, HTTP code, start of the answer, "send again"),
and renders a hook with the data of the last delivery of an event, or example data, without sending
it ("test"; secrets shown as `***`). Sent deliveries are kept 30 days, others 90.

## Events and data

Every event has `event`, `site` (`title`, `url`), `remote` (`number`, `e164`, `name`) and `contact`:
the matching contact for the remote number, from the person's own contacts, the shared phone book or
an external phone book (`source`: personal, shared, external; `name`, `organization`, `email`, for
external ones also `book`, `book_name`, `url` and `x`: the card's X- properties, e.g.
`X-DOLIBARR-EXTRA-EXT-CRM-ID` → `contact.x.dolibarr_extra_ext_crm_id`). `contact` is empty when
nothing matches.

**`call.ended`** – two minutes after every call that reached a person, group or voice menu or went
out over a trunk (recorded call flow; the wait lets notes written right after hanging up come along):

| | |
|---|---|
| `call.id` | Asterisk linkedid |
| `call.direction` | `in`, `out`, `internal`; also `call.incoming`, `call.outgoing`, `call.internal`, `call.external` (true/false) |
| `call.answered`, `call.voicemail` | true/false |
| `call.started`, `call.ended` | UTC, ISO 8601 (`2026-01-02T09:00:00Z`) |
| `call.started_local`, `call.ended_local` | site time zone (`2026-01-02 10:00:00`) |
| `call.started_epoch`, `call.ended_epoch` | Unix time |
| `call.duration_s`, `call.talk_s` | seconds (whole call, conversation) |
| `call.did`, `call.trunk` | own number it came in on (E.164), trunk name |
| `call.steps` | way through the PBX: `kind` (user/group/ivr), `number`, `state` |
| `user` | the person: caller (outgoing, also click-to-call), who answered or was rung first (incoming): `id`, `ext`, `name`, `email` |
| `device` | `label`, `number` of the device used |
| `notes` | call notes on the number written during the call or in the two minutes after it: `body`, `shared`, `author`, `author_id`; shared ones, with `private_notes = true` also the person's own |

**`note.created`, `note.updated`, `note.deleted`** – call notes (call window, contact page): `note`
(`id`, `body`, `shared`, `at`, `at_local`, `at_epoch`), `user` (the author). Notes only their author
sees are sent only to hooks with `private_notes = true`.

**`voicemail.new`** – `voicemail` (`id`, `duration_s`, `owner`, `at`, `at_local`).

## Examples

These are starting points, not tested against the systems; check field names with the APIs.

### Pipedrive: call log on the person or company (CardDAV book from doli-carddav-bridge)

The Pipedrive id is in a Dolibarr extra field (`ext_crm_id`), so the external phone book carries it.

```toml
[[webhook]]
name = "pipedrive-calllog"
events = ["call.ended"]
when = "call.external and contact.x.dolibarr_soc_extra_ext_crm_id"
url = "https://api.pipedrive.com/v1/callLogs"
headers = { "x-api-token" = "{{ secret('pipedrive_token') }}", "Content-Type" = "application/json" }
body = '''
{
  "subject": {{ ("Anruf " ~ ("von " if call.incoming else "an ") ~ contact.name) | tojson }},
  "outcome": "{{ 'connected' if call.answered else 'no_answer' }}",
  "to_phone_number": {{ remote.e164 | tojson }},
  "start_time": "{{ call.started | replace('T', ' ') | replace('Z', '') }}",
  "end_time": "{{ call.ended | replace('T', ' ') | replace('Z', '') }}",
  "duration": "{{ call.talk_s }}",
  {% if contact.x.dolibarr_type == "contact" and contact.x.dolibarr_extra_ext_crm_id %}"person_id": {{ contact.x.dolibarr_extra_ext_crm_id | int }},{% endif %}
  "org_id": {{ contact.x.dolibarr_soc_extra_ext_crm_id | int }},
  "note": {{ (notes | map(attribute='body') | join('\n\n')) | tojson }}
}
'''
```

### Pipedrive: call notes as notes

```toml
[[webhook]]
name = "pipedrive-note"
events = ["note.created"]
when = "contact.x.dolibarr_soc_extra_ext_crm_id"
url = "https://api.pipedrive.com/v1/notes"
headers = { "x-api-token" = "{{ secret('pipedrive_token') }}", "Content-Type" = "application/json" }
body = '''
{
  "content": {{ ("Telefonnotiz von " ~ user.name ~ ": " ~ note.body) | tojson }},
  {% if contact.x.dolibarr_type == "contact" and contact.x.dolibarr_extra_ext_crm_id %}"person_id": {{ contact.x.dolibarr_extra_ext_crm_id | int }}{% else %}"org_id": {{ contact.x.dolibarr_soc_extra_ext_crm_id | int }}{% endif %}
}
'''
```

### Dolibarr: call as agenda event

```toml
[[webhook]]
name = "dolibarr-agenda"
events = ["call.ended"]
when = "call.external and call.answered and contact.x.dolibarr_type"
url = "https://dolibarr.example.org/api/index.php/agendaevents"
headers = { DOLAPIKEY = "{{ secret('dolibarr_api_key') }}", "Content-Type" = "application/json" }
body = '''
{
  "type_code": "AC_TEL",
  "label": {{ ("Anruf " ~ ("von " if call.incoming else "an ") ~ contact.name ~ " (" ~ user.name ~ ")") | tojson }},
  "datep": {{ call.started_epoch }}, "datef": {{ call.ended_epoch }},
  "percentage": 100,
  "socid": {{ (contact.x.dolibarr_socid or contact.x.dolibarr_id) | int }},
  {% if contact.x.dolibarr_type == "contact" %}"contact_id": {{ contact.x.dolibarr_id | int }},{% endif %}
  "note_private": {{ (notes | map(attribute='body') | join('\n\n')) | tojson }}
}
'''
```

(Writing agenda events needs a Dolibarr user with the right to create them; the read-only user of
doli-carddav-bridge cannot.)

### Chat: missed external calls

```toml
[[webhook]]
name = "chat-missed"
events = ["call.ended"]
when = "call.incoming and not call.answered and not call.voicemail"
url = "https://chat.example.org/hooks/{{ secret('chat_hook_id') | urlencode }}"
headers = { "Content-Type" = "application/json" }
body = '''{ "text": {{ ("Verpasst: " ~ (contact.name or remote.number) ~ " auf " ~ (call.did or "?")) | tojson }} }'''
```
