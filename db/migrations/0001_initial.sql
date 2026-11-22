-- SPDX-License-Identifier: GPL-3.0-or-later
-- Database schema of patchbay-web and Asterisk (realtime, func_odbc), as of 1.0.
-- Design and the reasons behind it: docs/architecture.md (data model, security model).
-- Later changes come as further numbered files.
--
-- Roles: the migrations run as the database owner (pbx). patchbay-web uses the role pbxweb (data, no
-- DDL), Asterisk the role asterisk (reads only what the dialplan and realtime need, writes CDR).
-- Their passwords are set by `patchbay-web migrate` from secret files.

-- SQL functions refer to tables created further down
SET LOCAL check_function_bodies = false;

DO $$ BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'pbxweb') THEN
        CREATE ROLE pbxweb LOGIN;
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'asterisk') THEN
        CREATE ROLE asterisk LOGIN;
    END IF;
    EXECUTE format('GRANT CONNECT ON DATABASE %I TO pbxweb', current_database());
END $$;
-- ================================================================================================
-- Types
-- ================================================================================================

CREATE TYPE clip_mode AS ENUM (
    'user',
    'route',
    'fixed'
);

CREATE TYPE device_type AS ENUM (
    'linphone',
    'dect',
    'deskphone',
    'generic'
);

CREATE TYPE inbound_mode AS ENUM (
    'did',
    'passthrough'
);

CREATE TYPE managed_by AS ENUM (
    'self',
    'admin'
);

CREATE TYPE route_action AS ENUM (
    'route',
    'deny',
    'local'
);

CREATE TYPE sip_transport AS ENUM (
    'tls',
    'udp',
    'tcp'
);

CREATE TYPE target_kind AS ENUM (
    'user',
    'group',
    'ivr',
    'mailbox_user',
    'mailbox_group',
    'external',
    'announcement',
    'hangup',
    'app'
);

CREATE TYPE trunk_kind AS ENUM (
    'sip_register',
    'sip_ip',
    'iax2'
);

CREATE TYPE user_kind AS ENUM (
    'person',
    'function'
);

-- ================================================================================================
-- Functions
-- ================================================================================================

CREATE FUNCTION caller_blocked(num text) RETURNS text
    LANGUAGE plpgsql SECURITY DEFINER
    SET search_path TO 'public'
    AS $$
DECLARE
    e text := caller_e164(num);
    hit integer;
BEGIN
    IF regexp_replace(COALESCE(num, ''), '[^0-9]', '', 'g') = '' THEN
        RETURN CASE WHEN (SELECT value FROM settings WHERE key = 'block_anonymous') = 'true' THEN '1' ELSE '' END;
    END IF;
    IF e IS NULL THEN
        RETURN '';
    END IF;
    UPDATE blocked_numbers SET hits = hits + 1, last_hit = now()
     WHERE id = (SELECT id FROM blocked_numbers
                  WHERE pattern = e OR (pattern LIKE '%*' AND e LIKE rtrim(pattern, '*') || '%')
                  ORDER BY length(pattern) DESC LIMIT 1)
    RETURNING id INTO hit;
    RETURN CASE WHEN hit IS NULL THEN '' ELSE '1' END;
END
$$;

CREATE FUNCTION caller_e164(num text) RETURNS text
    LANGUAGE sql STABLE
    SET search_path TO 'public'
    AS $_$
    WITH c AS (
        SELECT regexp_replace(COALESCE(num, ''), '[^0-9+]', '', 'g') AS raw,
               (SELECT value FROM settings WHERE key = 'country_code') AS cc
    ), i AS (
        SELECT cc, CASE WHEN raw LIKE '00%' THEN '+' || substr(raw, 3) ELSE raw END AS n FROM c
    )
    SELECT CASE
        WHEN cc IS NOT NULL AND n LIKE '+' || cc || '0%' THEN '+' || cc || substr(n, length(cc) + 3)
        WHEN n ~ '^\+[1-9][0-9]{5,14}$' THEN n
        WHEN cc IS NOT NULL AND n ~ '^0[1-9][0-9]{4,13}$' THEN '+' || cc || substr(n, 2)
    END FROM i
$_$;

CREATE FUNCTION contact_name(ext text, num text) RETURNS text
    LANGUAGE sql STABLE SECURITY DEFINER
    SET search_path TO 'public'
    AS $$
    -- country code from settings (written by patchbay-web from PBX_COUNTRY_CODE); without it national
    -- numbers (0…) cannot be converted and only match as written
    WITH c AS (
        SELECT regexp_replace(num, '[^0-9+]', '', 'g') AS raw,
               (SELECT value FROM settings WHERE key = 'country_code') AS cc
    ), i AS (
        -- 00 -> +, then the trunk 0 after the own country code goes: +CC0… -> +CC…
        SELECT raw, cc, CASE WHEN raw LIKE '00%' THEN '+' || substr(raw, 3) ELSE raw END AS n FROM c
    ), e AS (
        SELECT raw, CASE
            WHEN cc IS NOT NULL AND n LIKE '+' || cc || '0%' THEN '+' || cc || substr(n, length(cc) + 3)
            WHEN n LIKE '+%' THEN n
            WHEN n LIKE '0%' THEN '+' || cc || substr(n, 2)
        END AS e164 FROM i
    )
    -- no quotes, commas, backslashes or control characters: the name goes into CALLERID(name), and
    -- func_odbc would escape commas
    SELECT btrim(regexp_replace(k.name, '[[:cntrl:][:space:]",\\]+', ' ', 'g'))
    FROM contacts k JOIN users u ON u.id = k.user_id, e
    WHERE u.extension::text = ext AND u.deleted_at IS NULL AND length(e.raw) > 4
      AND (k.phone = e.raw OR k.mobile = e.raw OR k.phone_e164 = e.e164 OR k.mobile_e164 = e.e164)
    ORDER BY k.id
    LIMIT 1
$$;

CREATE FUNCTION may_dial(uid integer, n text) RETURNS boolean
    LANGUAGE sql STABLE
    AS $$
    SELECT COALESCE((
        SELECT r.action = 'route' AND r.trunk <> '' AND r.route_class = ANY (e.classes)
        FROM route_for(n) r, user_effective_classes e JOIN users u ON u.id = e.user_id
        WHERE e.user_id = uid AND u.enabled AND u.phone_allowed AND u.deleted_at IS NULL
    ), false)
$$;

CREATE FUNCTION numbers_sync() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    col   text := TG_ARGV[0];
    o     jsonb;
    n     jsonb;
    k_old text;
    k_new text;
    n_old smallint;
    n_new smallint;
    p     number_policy;
    cool  integer;
    held  timestamptz;
BEGIN
    IF TG_OP <> 'INSERT' THEN o := to_jsonb(OLD); END IF;
    IF TG_OP <> 'DELETE' THEN n := to_jsonb(NEW); END IF;
    IF o IS NOT NULL AND o->>'deleted_at' IS NULL AND o->>col IS NOT NULL THEN
        n_old := (o->>col)::smallint;
        k_old := CASE WHEN TG_ARGV[1] = 'app' AND o->>'owner_user_id' IS NULL THEN 'company_app' ELSE TG_ARGV[1] END;
    END IF;
    IF n IS NOT NULL AND n->>'deleted_at' IS NULL AND n->>col IS NOT NULL THEN
        n_new := (n->>col)::smallint;
        k_new := CASE WHEN TG_ARGV[1] = 'app' AND n->>'owner_user_id' IS NULL THEN 'company_app' ELSE TG_ARGV[1] END;
    END IF;
    IF n_old IS NOT DISTINCT FROM n_new AND k_old IS NOT DISTINCT FROM k_new THEN
        RETURN NULL;
    END IF;
    IF n_old IS NOT NULL THEN
        DELETE FROM numbers WHERE number = n_old AND kind = k_old AND ref_id = (o->>'id')::int;
        INSERT INTO number_holds (number, kind, ref_id) VALUES (n_old, k_old, (o->>'id')::int)
        ON CONFLICT (number) DO UPDATE SET kind = EXCLUDED.kind, ref_id = EXCLUDED.ref_id, released_at = now();
    END IF;
    IF n_new IS NOT NULL THEN
        IF n_new NOT BETWEEN 1000 AND 9999 THEN
            RAISE EXCEPTION 'Number %: internal numbers have 4 digits (1000–9999).', n_new USING ERRCODE = 'check_violation';
        END IF;
        SELECT * INTO p FROM number_policy WHERE kind = k_new;
        IF FOUND AND (n_new < p.first OR n_new > p.last) THEN
            RAISE EXCEPTION 'Number % is outside the range %–%.', n_new, p.first, p.last USING ERRCODE = 'check_violation';
        END IF;
        cool := COALESCE((SELECT value::int FROM settings WHERE key = 'number_cooldown_days'), 90);
        SELECT h.released_at INTO held FROM number_holds h
        WHERE h.number = n_new AND h.released_at > now() - make_interval(days => cool)
          AND NOT (h.kind = k_new AND h.ref_id = (n->>'id')::int);
        IF FOUND THEN
            RAISE EXCEPTION 'Number % was released recently and is blocked until %.', n_new,
                to_char(held + make_interval(days => cool), 'DD.MM.YYYY') USING ERRCODE = 'check_violation';
        END IF;
        DELETE FROM number_holds WHERE number = n_new;
        BEGIN
            INSERT INTO numbers (number, kind, ref_id) VALUES (n_new, k_new, (n->>'id')::int);
        EXCEPTION WHEN unique_violation THEN
            RAISE EXCEPTION 'Number % is already in use.', n_new USING ERRCODE = 'unique_violation';
        END;
    END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION route_for(n text) RETURNS TABLE(route_id integer, action text, trunk text, strip_digits smallint, prepend text, route_class text, clip_mode text, clip_e164 text, trunk_kind text, max_channels integer)
    LANGUAGE sql STABLE
    AS $$
    SELECT r.id, r.action::text, COALESCE(t.asterisk_ref, ''), r.strip_digits, r.prepend, r.route_class,
           r.clip_mode::text, COALESCE(d.e164, ''), COALESCE(t.kind::text, ''), COALESCE(t.max_channels, 0)::integer
    FROM outbound_routes r
    LEFT JOIN trunks t ON t.id = r.trunk_id AND t.enabled AND t.deleted_at IS NULL
    LEFT JOIN dids d ON d.id = r.clip_did_id
    WHERE n LIKE r.match_prefix || '%' AND length(n) >= r.min_len AND (r.max_len IS NULL OR length(n) <= r.max_len)
    ORDER BY r.priority, length(r.match_prefix) DESC
    LIMIT 1
$$;

CREATE FUNCTION time_profile_state(p integer) RETURNS text
    LANGUAGE sql STABLE SECURITY DEFINER
    SET search_path TO 'public'
    AS $$
    SELECT CASE
        WHEN EXISTS (SELECT 1 FROM holidays h WHERE h.region = tp.holiday_region AND NOT h.hidden
                       AND h.date = (now() AT TIME ZONE tp.timezone)::date)
          OR EXISTS (SELECT 1 FROM profile_closures c WHERE c.profile_id = tp.id
                       AND c.date = (now() AT TIME ZONE tp.timezone)::date)
            THEN 'holiday'
        WHEN EXISTS (SELECT 1 FROM time_rules r WHERE r.profile_id = tp.id
                       AND (r.weekdays >> (extract(isodow FROM now() AT TIME ZONE tp.timezone)::int - 1)) & 1 = 1
                       AND (now() AT TIME ZONE tp.timezone)::time >= r.from_time
                       AND (now() AT TIME ZONE tp.timezone)::time < r.to_time)
            THEN 'open'
        ELSE 'closed'
    END
    FROM time_profiles tp WHERE tp.id = p
$$;

CREATE FUNCTION user_caller_blocked(ext text, num text) RETURNS text
    LANGUAGE plpgsql SECURITY DEFINER
    SET search_path TO 'public'
    AS $$
DECLARE
    e text := caller_e164(num);
    hit integer;
BEGIN
    IF e IS NULL THEN
        RETURN '';
    END IF;
    UPDATE user_blocked_numbers b SET hits = b.hits + 1, last_hit = now()
      FROM users u
     WHERE u.id = b.user_id AND u.extension::text = ext AND u.deleted_at IS NULL AND b.e164 = e
    RETURNING b.id INTO hit;
    RETURN CASE WHEN hit IS NULL THEN '' ELSE '1' END;
END
$$;

-- ================================================================================================
-- Tables and views
-- ================================================================================================

CREATE TABLE devices (
    id integer NOT NULL,
    user_id integer NOT NULL,
    number smallint,
    label text NOT NULL,
    type device_type NOT NULL,
    managed_by managed_by DEFAULT 'self'::managed_by NOT NULL,
    sip_username text NOT NULL,
    ha1 text NOT NULL,
    transport sip_transport DEFAULT 'tls'::sip_transport NOT NULL,
    enabled boolean DEFAULT true NOT NULL,
    ring boolean DEFAULT true NOT NULL,
    ring_delay_s smallint DEFAULT 0 NOT NULL,
    ring_order smallint DEFAULT 0 NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    deleted_at timestamp with time zone,
    srtp text DEFAULT 'required'::text NOT NULL,
    admin_locked boolean DEFAULT false NOT NULL,
    contact_uri text,
    CONSTRAINT devices_contact_uri_check CHECK ((contact_uri ~ '^sip:([A-Za-z0-9._~+-]+@)?[A-Za-z0-9.-]+(:[0-9]{1,5})?$'::text)),
    CONSTRAINT devices_ha1_check CHECK ((ha1 ~ '^[0-9a-f]{32}$'::text)),
    CONSTRAINT devices_ring_delay_s_check CHECK (((ring_delay_s >= 0) AND (ring_delay_s <= 120))),
    CONSTRAINT devices_sip_username_check CHECK ((sip_username ~ '^[a-z0-9]{8,32}$'::text)),
    CONSTRAINT devices_srtp_check CHECK ((srtp = ANY (ARRAY['required'::text, 'optional'::text, 'off'::text])))
);

CREATE TABLE users (
    id integer NOT NULL,
    kind user_kind DEFAULT 'person'::user_kind NOT NULL,
    extension smallint NOT NULL,
    display_name text NOT NULL,
    email text,
    mobile_e164 text,
    kc_subject text,
    enabled boolean DEFAULT true NOT NULL,
    phone_allowed boolean DEFAULT true NOT NULL,
    route_classes text[] DEFAULT '{internal,national,mobile}'::text[] NOT NULL,
    outbound_did_id integer,
    language text DEFAULT 'de'::text NOT NULL,
    dnd boolean DEFAULT false NOT NULL,
    ring_timeout_s smallint DEFAULT 25 NOT NULL,
    pickup_group text DEFAULT 'all'::text,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    deleted_at timestamp with time zone,
    forward_immediate integer,
    no_answer_target integer,
    kc_groups text[] DEFAULT '{}'::text[] NOT NULL,
    allow_sip_devices boolean DEFAULT false NOT NULL,
    ui_language text,
    in_phonebook boolean DEFAULT true NOT NULL,
    number_changed_at timestamp with time zone,
    web_access boolean DEFAULT false NOT NULL,
    web_admin boolean DEFAULT false NOT NULL,
    CONSTRAINT users_language_check CHECK ((language = ANY (ARRAY['de'::text, 'en'::text]))),
    CONSTRAINT users_mobile_e164_check CHECK ((mobile_e164 ~ '^\+[1-9][0-9]{6,14}$'::text)),
    CONSTRAINT users_ring_timeout_s_check CHECK (((ring_timeout_s >= 5) AND (ring_timeout_s <= 300))),
    CONSTRAINT users_ui_language_check CHECK ((ui_language = ANY (ARRAY['en'::text, 'de'::text])))
);

CREATE VIEW active_devices AS
 SELECT d.id,
    d.user_id,
    d.number,
    d.label,
    d.type,
    d.managed_by,
    d.sip_username,
    d.ha1,
    d.transport,
    d.enabled,
    d.ring,
    d.ring_delay_s,
    d.ring_order,
    d.created_at,
    d.deleted_at,
    d.srtp,
    u.extension,
    u.display_name,
    u.language,
    u.pickup_group,
    d.contact_uri
   FROM (devices d
     JOIN users u ON ((u.id = d.user_id)))
  WHERE (d.enabled AND (d.deleted_at IS NULL) AND u.enabled AND u.phone_allowed AND (u.deleted_at IS NULL));

CREATE TABLE app_media (
    app_id integer NOT NULL,
    sound_id integer NOT NULL,
    "position" smallint DEFAULT 0 NOT NULL
);

CREATE TABLE apps (
    id integer NOT NULL,
    owner_user_id integer,
    number smallint NOT NULL,
    name text NOT NULL,
    kind text NOT NULL,
    enabled boolean DEFAULT true NOT NULL,
    config jsonb DEFAULT '{}'::jsonb NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    deleted_at timestamp with time zone,
    CONSTRAINT apps_kind_check CHECK ((kind = ANY (ARRAY['conference'::text, 'music'::text, 'announcement'::text, 'echo'::text, 'alias'::text, 'clock'::text, 'group'::text, 'script'::text, 'gateway'::text])))
);

CREATE TABLE settings (
    key text NOT NULL,
    value text NOT NULL
);

CREATE VIEW app_dialplan AS
 SELECT a.id,
    a.number,
    a.kind,
    COALESCE((a.config ->> 'pin'::text), ''::text) AS pin,
    COALESCE((a.config ->> 'admin_pin'::text), ''::text) AS admin_pin,
    COALESCE(((a.config ->> 'max_members'::text))::integer, 20) AS max_members,
        CASE
            WHEN (EXISTS ( SELECT 1
               FROM app_media m
              WHERE (m.app_id = a.id))) THEN ('app'::text || a.id)
            ELSE 'default'::text
        END AS moh_class,
    COALESCE(( SELECT ('pbx/'::text || m.sound_id)
           FROM app_media m
          WHERE (m.app_id = a.id)
          ORDER BY m."position"
         LIMIT 1), ''::text) AS sound_file,
    COALESCE(((a.config ->> 'repeat'::text))::integer, 1) AS repeat,
    COALESCE((a.config ->> 'target'::text), ''::text) AS target
   FROM (apps a
     LEFT JOIN users u ON ((u.id = a.owner_user_id)))
  WHERE (a.enabled AND (a.deleted_at IS NULL) AND ((a.owner_user_id IS NULL) OR ((a.kind <> 'gateway'::text) AND u.enabled AND (u.deleted_at IS NULL) AND (a.kind = ANY (COALESCE(string_to_array(( SELECT settings.value
           FROM settings
          WHERE (settings.key = 'user_app_kinds'::text)), ','::text), ARRAY[a.kind]))))));

CREATE TABLE app_members (
    app_id integer NOT NULL,
    user_id integer NOT NULL,
    added_at timestamp with time zone DEFAULT now() NOT NULL
);

CREATE VIEW app_group AS
 SELECT id,
    LEAST(GREATEST(COALESCE(((config ->> 'ring_s'::text))::integer, 30), 10), 120) AS ring_s,
    COALESCE((config ->> 'prefix'::text), ''::text) AS prefix,
    COALESCE(( SELECT string_agg((u.extension)::text, '-'::text ORDER BY m.added_at, u.extension) AS string_agg
           FROM (app_members m
             JOIN users u ON ((u.id = m.user_id)))
          WHERE ((m.app_id = a.id) AND u.enabled AND u.phone_allowed AND (NOT u.dnd) AND (u.deleted_at IS NULL) AND (EXISTS ( SELECT 1
                   FROM active_devices d
                  WHERE ((d.user_id = u.id) AND d.ring))))), ''::text) AS members
   FROM apps a
  WHERE ((kind = 'group'::text) AND (deleted_at IS NULL));

CREATE SEQUENCE apps_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE apps_id_seq OWNED BY apps.id;

CREATE TABLE audit_log (
    id bigint NOT NULL,
    at timestamp with time zone DEFAULT now() NOT NULL,
    actor text NOT NULL,
    action text NOT NULL,
    object text NOT NULL,
    details jsonb DEFAULT '{}'::jsonb NOT NULL
);

CREATE SEQUENCE audit_log_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE audit_log_id_seq OWNED BY audit_log.id;

CREATE TABLE blocked_numbers (
    id integer NOT NULL,
    pattern text NOT NULL,
    note text DEFAULT ''::text NOT NULL,
    created_by text DEFAULT ''::text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    hits integer DEFAULT 0 NOT NULL,
    last_hit timestamp with time zone,
    CONSTRAINT blocked_numbers_note_check CHECK ((char_length(note) <= 200)),
    CONSTRAINT blocked_numbers_pattern_check CHECK ((pattern ~ '^\+[1-9][0-9]{1,14}\*?$'::text))
);

CREATE SEQUENCE blocked_numbers_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE blocked_numbers_id_seq OWNED BY blocked_numbers.id;

CREATE TABLE bot_group_channels (
    platform text NOT NULL,
    channel text NOT NULL,
    group_id integer NOT NULL,
    linked_by integer,
    linked_at timestamp with time zone DEFAULT now() NOT NULL
);

CREATE TABLE bot_link_codes (
    code_hash text NOT NULL,
    user_id integer NOT NULL,
    expires_at timestamp with time zone NOT NULL
);

CREATE TABLE bot_links (
    platform text NOT NULL,
    chat_user text NOT NULL,
    chat_name text DEFAULT ''::text NOT NULL,
    user_id integer NOT NULL,
    via text NOT NULL,
    linked_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT bot_links_via_check CHECK ((via = ANY (ARRAY['auto'::text, 'code'::text])))
);

CREATE TABLE bot_user_prefs (
    user_id integer NOT NULL,
    vm_delivery text DEFAULT 'mail'::text NOT NULL,
    CONSTRAINT bot_user_prefs_vm_delivery_check CHECK ((vm_delivery = ANY (ARRAY['mail'::text, 'chat'::text, 'both'::text, 'none'::text])))
);

CREATE TABLE call_notes (
    id integer NOT NULL,
    user_id integer NOT NULL,
    e164 text NOT NULL,
    body text NOT NULL,
    shared boolean DEFAULT false NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT call_notes_body_check CHECK (((char_length(body) >= 1) AND (char_length(body) <= 1000))),
    CONSTRAINT call_notes_e164_check CHECK ((e164 ~ '^\+[1-9][0-9]{5,14}$'::text))
);

CREATE SEQUENCE call_notes_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE call_notes_id_seq OWNED BY call_notes.id;

CREATE TABLE cdr (
    id bigint NOT NULL,
    calldate timestamp with time zone DEFAULT now() NOT NULL,
    start timestamp with time zone,
    answer timestamp with time zone,
    end_time timestamp with time zone,
    clid text,
    src text,
    dst text,
    dcontext text,
    channel text,
    dstchannel text,
    lastapp text,
    lastdata text,
    duration integer,
    billsec integer,
    disposition text,
    amaflags integer,
    accountcode text,
    uniqueid text,
    linkedid text,
    userfield text,
    peeraccount text,
    sequence integer
);

CREATE SEQUENCE cdr_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE cdr_id_seq OWNED BY cdr.id;

CREATE TABLE contacts (
    id integer NOT NULL,
    user_id integer NOT NULL,
    name text NOT NULL,
    organization text,
    phone text,
    phone_e164 text,
    mobile text,
    mobile_e164 text,
    email text,
    note text,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    vcard text,
    dav_name text,
    CONSTRAINT contacts_name_check CHECK (((length(name) >= 1) AND (length(name) <= 80)))
);

CREATE SEQUENCE contacts_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE contacts_id_seq OWNED BY contacts.id;

CREATE TABLE custom_apps (
    id integer NOT NULL,
    number smallint NOT NULL,
    name text NOT NULL,
    template text DEFAULT ''::text NOT NULL,
    rendered text DEFAULT ''::text NOT NULL,
    enabled boolean DEFAULT true NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    deleted_at timestamp with time zone
);

CREATE SEQUENCE custom_apps_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE custom_apps_id_seq OWNED BY custom_apps.id;

CREATE TABLE dav_tokens (
    id integer NOT NULL,
    user_id integer NOT NULL,
    label text NOT NULL,
    ha1 text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    last_used_at timestamp with time zone,
    CONSTRAINT dav_tokens_label_check CHECK (((length(label) >= 1) AND (length(label) <= 60)))
);

CREATE SEQUENCE dav_tokens_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE dav_tokens_id_seq OWNED BY dav_tokens.id;

CREATE SEQUENCE devices_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE devices_id_seq OWNED BY devices.id;

CREATE TABLE dids (
    id integer NOT NULL,
    trunk_id integer NOT NULL,
    e164 text NOT NULL,
    label text DEFAULT ''::text NOT NULL,
    CONSTRAINT dids_e164_check CHECK ((e164 ~ '^\+[1-9][0-9]{6,14}$'::text))
);

CREATE SEQUENCE dids_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE dids_id_seq OWNED BY dids.id;

CREATE TABLE greetings (
    owner_type text NOT NULL,
    owner_id integer NOT NULL,
    kind text NOT NULL,
    sound_id integer NOT NULL,
    CONSTRAINT greetings_kind_check CHECK ((kind = ANY (ARRAY['unavailable'::text, 'busy'::text, 'closed'::text]))),
    CONSTRAINT greetings_owner_type_check CHECK ((owner_type = ANY (ARRAY['user'::text, 'group'::text])))
);

CREATE TABLE group_members (
    group_id integer NOT NULL,
    user_id integer NOT NULL,
    priority smallint DEFAULT 0 NOT NULL,
    enabled boolean DEFAULT true NOT NULL,
    logged_in boolean DEFAULT true NOT NULL
);

CREATE VIEW group_ringing_members AS
 SELECT m.group_id,
    u.id AS user_id,
    u.extension,
    u.display_name,
    m.priority
   FROM (group_members m
     JOIN users u ON ((u.id = m.user_id)))
  WHERE (m.enabled AND m.logged_in AND u.enabled AND u.phone_allowed AND (NOT u.dnd) AND (u.deleted_at IS NULL) AND (EXISTS ( SELECT 1
           FROM active_devices a
          WHERE ((a.user_id = u.id) AND a.ring))));

CREATE TABLE groups (
    id integer NOT NULL,
    number smallint NOT NULL,
    name text NOT NULL,
    strategy text DEFAULT 'ringall'::text NOT NULL,
    member_timeout_s smallint DEFAULT 20 NOT NULL,
    total_timeout_s smallint DEFAULT 120 NOT NULL,
    moh boolean DEFAULT true NOT NULL,
    language text DEFAULT 'de'::text NOT NULL,
    deleted_at timestamp with time zone,
    no_answer_target integer,
    announce_sound_id integer,
    moh_playlist_id integer,
    caller_name_prefix text DEFAULT ''::text NOT NULL,
    queue boolean DEFAULT false NOT NULL,
    queue_announce_s smallint DEFAULT 30 NOT NULL,
    CONSTRAINT groups_caller_name_prefix_check CHECK (((char_length(caller_name_prefix) <= 8) AND (caller_name_prefix !~ '[,;:"\\|<>[:cntrl:]]'::text))),
    CONSTRAINT groups_queue_announce_s_check CHECK (((queue_announce_s = 0) OR ((queue_announce_s >= 15) AND (queue_announce_s <= 300)))),
    CONSTRAINT groups_strategy_check CHECK ((strategy = ANY (ARRAY['ringall'::text, 'rrmemory'::text, 'linear'::text])))
);

CREATE SEQUENCE groups_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE groups_id_seq OWNED BY groups.id;

CREATE TABLE holiday_sources (
    id integer NOT NULL,
    kind text NOT NULL,
    country text DEFAULT ''::text NOT NULL,
    subdivision text DEFAULT ''::text NOT NULL,
    url text,
    keep_updated boolean DEFAULT false NOT NULL,
    last_run timestamp with time zone,
    last_result text DEFAULT ''::text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT holiday_sources_kind_check CHECK ((kind = ANY (ARRAY['nager'::text, 'ical'::text])))
);

CREATE SEQUENCE holiday_sources_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE holiday_sources_id_seq OWNED BY holiday_sources.id;

CREATE TABLE holidays (
    region text NOT NULL,
    date date NOT NULL,
    name text NOT NULL,
    source_id integer,
    hidden boolean DEFAULT false NOT NULL
);

CREATE TABLE inbound_rules (
    did_id integer NOT NULL,
    time_profile_id integer,
    target_open integer NOT NULL,
    target_closed integer,
    target_holiday integer
);

CREATE TABLE ivr_options (
    ivr_id integer NOT NULL,
    digit text NOT NULL,
    target_id integer NOT NULL,
    CONSTRAINT ivr_options_digit_check CHECK ((digit ~ '^[0-9*#]$'::text))
);

CREATE TABLE ivrs (
    id integer NOT NULL,
    name text NOT NULL,
    language text DEFAULT 'de'::text NOT NULL,
    greeting integer,
    timeout_s smallint DEFAULT 7 NOT NULL,
    repeat smallint DEFAULT 2 NOT NULL,
    timeout_target integer,
    deleted_at timestamp with time zone,
    number smallint,
    invalid_mode text DEFAULT 'announce'::text NOT NULL,
    CONSTRAINT ivrs_invalid_mode_check CHECK ((invalid_mode = ANY (ARRAY['announce'::text, 'ignore'::text])))
);

CREATE SEQUENCE ivrs_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE ivrs_id_seq OWNED BY ivrs.id;

CREATE TABLE local_logins (
    user_id integer NOT NULL,
    login text NOT NULL,
    password_hash text NOT NULL,
    password_changed_at timestamp with time zone DEFAULT now() NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT local_logins_login_check CHECK ((login ~ '^[a-z0-9][a-z0-9._@-]{1,63}$'::text)),
    CONSTRAINT local_logins_password_hash_check CHECK ((password_hash ~~ '$argon2%'::text))
);

CREATE TABLE moh_media (
    playlist_id integer NOT NULL,
    sound_id integer NOT NULL,
    "position" smallint DEFAULT 0 NOT NULL
);

CREATE TABLE moh_playlists (
    id integer NOT NULL,
    owner_user_id integer,
    name text NOT NULL,
    shuffle boolean DEFAULT true NOT NULL,
    is_default boolean DEFAULT false NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT moh_playlists_check CHECK (((NOT is_default) OR (owner_user_id IS NULL))),
    CONSTRAINT moh_playlists_name_check CHECK (((length(name) >= 1) AND (length(name) <= 40)))
);

CREATE SEQUENCE moh_playlists_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE moh_playlists_id_seq OWNED BY moh_playlists.id;

CREATE VIEW musiconhold AS
 SELECT ('app'::text || a.id) AS name,
    'files'::text AS mode,
    ('/var/lib/asterisk/moh-apps/app'::text || a.id) AS directory,
        CASE
            WHEN COALESCE(((a.config ->> 'shuffle'::text))::boolean, false) THEN 'random'::text
            ELSE 'alpha'::text
        END AS sort
   FROM apps a
  WHERE ((a.deleted_at IS NULL) AND (EXISTS ( SELECT 1
           FROM app_media m
          WHERE (m.app_id = a.id))))
UNION ALL
 SELECT ('pl'::text || p.id) AS name,
    'files'::text AS mode,
    ('/var/lib/asterisk/moh-apps/pl'::text || p.id) AS directory,
        CASE
            WHEN p.shuffle THEN 'random'::text
            ELSE 'alpha'::text
        END AS sort
   FROM moh_playlists p
  WHERE ((NOT p.is_default) AND (EXISTS ( SELECT 1
           FROM moh_media m
          WHERE (m.playlist_id = p.id))));

CREATE TABLE number_holds (
    number smallint NOT NULL,
    kind text NOT NULL,
    ref_id integer NOT NULL,
    released_at timestamp with time zone DEFAULT now() NOT NULL
);

CREATE TABLE number_policy (
    kind text NOT NULL,
    first smallint DEFAULT 1000 NOT NULL,
    last smallint DEFAULT 9999 NOT NULL,
    strategy text DEFAULT 'next'::text NOT NULL,
    enabled boolean DEFAULT true NOT NULL,
    CONSTRAINT number_policy_check CHECK ((((first >= 1000) AND (first <= 9999)) AND ((last >= first) AND (last <= 9999)))),
    CONSTRAINT number_policy_kind_check CHECK ((kind = ANY (ARRAY['person'::text, 'device'::text, 'group'::text, 'ivr'::text, 'custom_app'::text, 'app'::text, 'company_app'::text]))),
    CONSTRAINT number_policy_strategy_check CHECK ((strategy = ANY (ARRAY['next'::text, 'random'::text, 'choose'::text])))
);

CREATE TABLE numbers (
    number smallint NOT NULL,
    kind text NOT NULL,
    ref_id integer NOT NULL,
    CONSTRAINT numbers_number_check CHECK (((number >= 1000) AND (number <= 9999)))
);

CREATE TABLE outbound_routes (
    id integer NOT NULL,
    priority smallint NOT NULL,
    name text NOT NULL,
    match_prefix text NOT NULL,
    min_len smallint DEFAULT 1 NOT NULL,
    max_len smallint,
    action route_action NOT NULL,
    trunk_id integer,
    strip_digits smallint DEFAULT 0 NOT NULL,
    prepend text DEFAULT ''::text NOT NULL,
    clip_mode clip_mode DEFAULT 'user'::clip_mode NOT NULL,
    clip_did_id integer,
    route_class text DEFAULT 'national'::text NOT NULL,
    CONSTRAINT outbound_routes_check CHECK (((action <> 'route'::route_action) OR (trunk_id IS NOT NULL))),
    CONSTRAINT outbound_routes_no_fixed CHECK ((clip_mode <> 'fixed'::clip_mode)),
    CONSTRAINT outbound_routes_no_local CHECK ((action <> 'local'::route_action))
);

CREATE SEQUENCE outbound_routes_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE outbound_routes_id_seq OWNED BY outbound_routes.id;

CREATE TABLE profile_closures (
    profile_id integer NOT NULL,
    date date NOT NULL,
    name text DEFAULT ''::text NOT NULL
);

CREATE TABLE provisioning_tokens (
    id integer NOT NULL,
    device_id integer NOT NULL,
    kind text NOT NULL,
    token_sha256 text NOT NULL,
    expires_at timestamp with time zone,
    used_at timestamp with time zone,
    revoked_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT provisioning_tokens_kind_check CHECK ((kind = ANY (ARRAY['bootstrap'::text, 'device'::text]))),
    CONSTRAINT provisioning_tokens_token_sha256_check CHECK ((token_sha256 ~ '^[0-9a-f]{64}$'::text))
);

CREATE SEQUENCE provisioning_tokens_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE provisioning_tokens_id_seq OWNED BY provisioning_tokens.id;

CREATE VIEW ps_aors AS
 SELECT sip_username AS id,
    1 AS max_contacts,
    'yes'::text AS remove_existing,
        CASE
            WHEN (contact_uri IS NULL) THEN 60
            ELSE 0
        END AS qualify_frequency,
    format('%s@pbx'::text, extension) AS mailboxes,
    contact_uri AS contact
   FROM active_devices;

CREATE VIEW ps_auths AS
 SELECT sip_username AS id,
    'digest'::text AS auth_type,
    sip_username AS username,
    ( SELECT settings.value
           FROM settings
          WHERE (settings.key = 'sip_realm'::text)) AS realm,
    ('md5:'::text || ha1) AS password_digest
   FROM active_devices;

CREATE VIEW ps_endpoints AS
 SELECT sip_username AS id,
    ('transport-'::text || (transport)::text) AS transport,
    sip_username AS aors,
    sip_username AS auth,
    'from-device'::text AS context,
    'all'::text AS disallow,
        CASE
            WHEN (type = 'linphone'::device_type) THEN 'opus,g722,alaw,h264,vp8'::text
            ELSE 'g722,alaw,opus,h264,vp8'::text
        END AS allow,
    format('"%s" <%s>'::text, replace(display_name, '"'::text, ''::text), extension) AS callerid,
        CASE srtp
            WHEN 'off'::text THEN 'no'::text
            ELSE 'sdes'::text
        END AS media_encryption,
        CASE srtp
            WHEN 'optional'::text THEN 'yes'::text
            ELSE 'no'::text
        END AS media_encryption_optimistic,
        CASE
            WHEN ((transport)::text = 'tls'::text) THEN 'acl-devices-tls'::text
            ELSE 'acl-devices-udp'::text
        END AS acl,
    'rfc4733'::text AS dtmf_mode,
    language,
    'yes'::text AS rtp_symmetric,
    'yes'::text AS force_rport,
    'yes'::text AS rewrite_contact,
    'no'::text AS direct_media,
    'yes'::text AS allow_subscribe,
    'hints'::text AS subscribe_context,
    'yes'::text AS send_pai,
    'yes'::text AS trust_id_outbound,
    format('%s@pbx'::text, extension) AS mailboxes,
    pickup_group AS named_call_group,
    pickup_group AS named_pickup_group,
    format('PBX_USER_EXT=%s'::text, extension) AS set_var,
    COALESCE(( SELECT ('pl'::text || p.id)
           FROM moh_playlists p
          WHERE ((p.owner_user_id = d.user_id) AND (EXISTS ( SELECT 1
                   FROM moh_media m
                  WHERE (m.playlist_id = p.id))) AND (EXISTS ( SELECT 1
                   FROM settings s
                  WHERE ((s.key = 'user_moh'::text) AND (s.value = 'true'::text)))))), 'default'::text) AS moh_suggest,
    'yes'::text AS mwi_subscribe_replaces_unsolicited,
    'prefer: configured, operation: intersect, keep: first, transcode: allow'::text AS codec_prefs_incoming_offer
   FROM active_devices d;

CREATE VIEW queue_members_rt AS
 SELECT format('g%s-%s'::text, r.group_id, r.extension) AS uniqueid,
    ('g'::text || r.group_id) AS queue_name,
    format('Local/%s@group-member'::text, r.extension) AS interface,
    r.display_name AS membername,
    format('hint:%s@hints'::text, r.extension) AS state_interface,
    '0'::text AS penalty,
    '0'::text AS paused,
    'no'::text AS ringinuse,
    '0'::text AS wrapuptime,
    r.priority
   FROM (group_ringing_members r
     JOIN groups g ON ((g.id = r.group_id)))
  WHERE ((g.deleted_at IS NULL) AND g.queue)
  ORDER BY r.group_id, r.priority, r.extension;

CREATE VIEW queue_rt AS
 SELECT ('g'::text || g.id) AS name,
    g.strategy,
    (g.member_timeout_s)::text AS timeout,
    '2'::text AS retry,
    '0'::text AS wrapuptime,
    'no'::text AS ringinuse,
    'yes'::text AS autofill,
    'unavailable,invalid,unknown'::text AS joinempty,
    'unavailable,invalid,unknown'::text AS leavewhenempty,
    'yes'::text AS setinterfacevar,
        CASE
            WHEN (NOT g.moh) THEN 'default'::text
            WHEN ((p.id IS NULL) OR p.is_default) THEN 'default'::text
            ELSE ('pl'::text || p.id)
        END AS musiconhold,
        CASE
            WHEN (g.queue_announce_s > 0) THEN 'yes'::text
            ELSE 'no'::text
        END AS announce_position,
    (g.queue_announce_s)::text AS announce_frequency,
    '15'::text AS min_announce_frequency,
    'no'::text AS announce_holdtime,
    'yes'::text AS announce_position_only_up
   FROM (groups g
     LEFT JOIN moh_playlists p ON (((p.id = g.moh_playlist_id) AND (EXISTS ( SELECT 1
           FROM moh_media m
          WHERE (m.playlist_id = p.id))))))
  WHERE ((g.deleted_at IS NULL) AND g.queue);

CREATE TABLE route_classes (
    key text NOT NULL,
    name text NOT NULL,
    description text DEFAULT ''::text NOT NULL,
    default_on boolean DEFAULT false NOT NULL,
    builtin boolean DEFAULT false NOT NULL,
    kc_group text,
    CONSTRAINT route_classes_key_check CHECK ((key ~ '^[a-z][a-z0-9_]{1,30}$'::text))
);

CREATE TABLE sounds (
    id integer NOT NULL,
    name text NOT NULL,
    language text DEFAULT 'de'::text NOT NULL,
    file_path text NOT NULL,
    uploaded_by integer,
    duration_s numeric(8,1),
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    deleted_at timestamp with time zone,
    source_name text DEFAULT ''::text NOT NULL,
    owner_user_id integer
);

CREATE SEQUENCE sounds_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE sounds_id_seq OWNED BY sounds.id;

CREATE TABLE targets (
    id integer NOT NULL,
    kind target_kind NOT NULL,
    user_id integer,
    group_id integer,
    ivr_id integer,
    sound_id integer,
    external_e164 text,
    owner_user_id integer,
    app_id integer,
    CONSTRAINT targets_check CHECK ((((kind = ANY (ARRAY['user'::target_kind, 'mailbox_user'::target_kind])) AND (user_id IS NOT NULL) AND (group_id IS NULL) AND (ivr_id IS NULL) AND (external_e164 IS NULL) AND (app_id IS NULL)) OR ((kind = ANY (ARRAY['group'::target_kind, 'mailbox_group'::target_kind])) AND (group_id IS NOT NULL) AND (user_id IS NULL) AND (ivr_id IS NULL) AND (external_e164 IS NULL) AND (app_id IS NULL)) OR ((kind = 'ivr'::target_kind) AND (ivr_id IS NOT NULL) AND (user_id IS NULL) AND (group_id IS NULL) AND (external_e164 IS NULL) AND (app_id IS NULL)) OR ((kind = 'external'::target_kind) AND (external_e164 IS NOT NULL) AND (user_id IS NULL) AND (group_id IS NULL) AND (ivr_id IS NULL) AND (app_id IS NULL)) OR ((kind = 'announcement'::target_kind) AND (sound_id IS NOT NULL) AND (user_id IS NULL) AND (group_id IS NULL) AND (ivr_id IS NULL) AND (app_id IS NULL)) OR ((kind = 'hangup'::target_kind) AND (user_id IS NULL) AND (group_id IS NULL) AND (ivr_id IS NULL) AND (external_e164 IS NULL) AND (app_id IS NULL)) OR ((kind = 'app'::target_kind) AND (app_id IS NOT NULL) AND (user_id IS NULL) AND (group_id IS NULL) AND (ivr_id IS NULL) AND (sound_id IS NULL) AND (external_e164 IS NULL)))),
    CONSTRAINT targets_external_e164_check CHECK ((external_e164 ~ '^\+[1-9][0-9]{6,14}$'::text))
);

CREATE SEQUENCE targets_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE targets_id_seq OWNED BY targets.id;

CREATE TABLE time_profiles (
    id integer NOT NULL,
    name text NOT NULL,
    timezone text DEFAULT 'UTC'::text NOT NULL,
    holiday_region text DEFAULT 'site'::text NOT NULL,
    deleted_at timestamp with time zone
);

CREATE SEQUENCE time_profiles_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE time_profiles_id_seq OWNED BY time_profiles.id;

CREATE TABLE time_rules (
    profile_id integer NOT NULL,
    weekdays smallint NOT NULL,
    from_time time without time zone NOT NULL,
    to_time time without time zone NOT NULL,
    id integer NOT NULL,
    CONSTRAINT time_rules_check CHECK ((to_time > from_time)),
    CONSTRAINT time_rules_weekdays_check CHECK (((weekdays >= 1) AND (weekdays <= 127)))
);

CREATE SEQUENCE time_rules_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE time_rules_id_seq OWNED BY time_rules.id;

CREATE TABLE trunk_inbound_rules (
    id integer NOT NULL,
    trunk_id integer NOT NULL,
    priority smallint DEFAULT 50 NOT NULL,
    match_prefix text DEFAULT ''::text NOT NULL,
    min_len smallint DEFAULT 1 NOT NULL,
    max_len smallint,
    action text NOT NULL,
    kinds text[] DEFAULT '{person,group}'::text[] NOT NULL,
    target_id integer,
    CONSTRAINT trunk_inbound_rules_action_check CHECK ((action = ANY (ARRAY['internal'::text, 'target'::text, 'outbound'::text, 'reject'::text])))
);

CREATE SEQUENCE trunk_inbound_rules_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE trunk_inbound_rules_id_seq OWNED BY trunk_inbound_rules.id;

CREATE TABLE trunks (
    id integer NOT NULL,
    name text NOT NULL,
    kind trunk_kind NOT NULL,
    asterisk_ref text NOT NULL,
    enabled boolean DEFAULT true NOT NULL,
    inbound_mode inbound_mode DEFAULT 'did'::inbound_mode NOT NULL,
    max_channels integer,
    managed boolean DEFAULT false NOT NULL,
    host text,
    port integer,
    secret_enc text,
    encryption boolean DEFAULT true NOT NULL,
    iax_trunk boolean DEFAULT false NOT NULL,
    calltoken text DEFAULT 'yes'::text NOT NULL,
    site_prefix text,
    route_classes text[] DEFAULT '{internal}'::text[] NOT NULL,
    deleted_at timestamp with time zone,
    sip_transport text DEFAULT 'udp'::text NOT NULL,
    username text,
    from_user text,
    from_domain text,
    outbound_proxy text,
    media_encryption text DEFAULT 'no'::text NOT NULL,
    codecs text[] DEFAULT '{alaw,g722}'::text[] NOT NULL,
    identify_match text,
    reg_expiration integer DEFAULT 600 NOT NULL,
    iax_username text,
    CONSTRAINT trunks_calltoken_check CHECK ((calltoken = ANY (ARRAY['yes'::text, 'auto'::text]))),
    CONSTRAINT trunks_iax_username_check CHECK ((iax_username ~ '^[A-Za-z0-9_-]{1,40}$'::text)),
    CONSTRAINT trunks_media_encryption_check CHECK ((media_encryption = ANY (ARRAY['no'::text, 'sdes'::text]))),
    CONSTRAINT trunks_sip_transport_check CHECK ((sip_transport = ANY (ARRAY['udp'::text, 'tcp'::text, 'tls'::text]))),
    CONSTRAINT trunks_site_prefix_check CHECK ((site_prefix ~ '^[1-9][0-9]{1,3}$'::text))
);

CREATE SEQUENCE trunks_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE trunks_id_seq OWNED BY trunks.id;

CREATE TABLE user_blocked_numbers (
    id integer NOT NULL,
    user_id integer NOT NULL,
    e164 text NOT NULL,
    note text DEFAULT ''::text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    hits integer DEFAULT 0 NOT NULL,
    last_hit timestamp with time zone,
    CONSTRAINT user_blocked_numbers_e164_check CHECK ((e164 ~ '^\+[1-9][0-9]{5,14}$'::text)),
    CONSTRAINT user_blocked_numbers_note_check CHECK ((char_length(note) <= 200))
);

CREATE SEQUENCE user_blocked_numbers_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE user_blocked_numbers_id_seq OWNED BY user_blocked_numbers.id;

CREATE VIEW user_effective_classes AS
 SELECT id AS user_id,
    ARRAY( SELECT DISTINCT s.k
           FROM ( SELECT unnest(u.route_classes) AS k
                UNION
                 SELECT c.key
                   FROM route_classes c
                  WHERE ((c.kc_group IS NOT NULL) AND (c.kc_group = ANY (u.kc_groups)))) s
          ORDER BY s.k) AS classes
   FROM users u;

CREATE SEQUENCE users_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE users_id_seq OWNED BY users.id;

CREATE TABLE voicemails (
    id integer NOT NULL,
    uuid text,
    owner_type text NOT NULL,
    owner_id integer NOT NULL,
    caller_e164 text,
    caller_name text,
    did_id integer,
    received_at timestamp with time zone DEFAULT now() NOT NULL,
    duration_s integer,
    file_path text NOT NULL,
    heard_by integer,
    heard_at timestamp with time zone,
    deleted_at timestamp with time zone,
    CONSTRAINT voicemails_owner_type_check CHECK ((owner_type = ANY (ARRAY['user'::text, 'group'::text])))
);

CREATE SEQUENCE voicemails_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE voicemails_id_seq OWNED BY voicemails.id;

-- ================================================================================================
-- Defaults, keys and constraints
-- ================================================================================================

ALTER TABLE ONLY apps ALTER COLUMN id SET DEFAULT nextval('apps_id_seq'::regclass);

ALTER TABLE ONLY audit_log ALTER COLUMN id SET DEFAULT nextval('audit_log_id_seq'::regclass);

ALTER TABLE ONLY blocked_numbers ALTER COLUMN id SET DEFAULT nextval('blocked_numbers_id_seq'::regclass);

ALTER TABLE ONLY call_notes ALTER COLUMN id SET DEFAULT nextval('call_notes_id_seq'::regclass);

ALTER TABLE ONLY cdr ALTER COLUMN id SET DEFAULT nextval('cdr_id_seq'::regclass);

ALTER TABLE ONLY contacts ALTER COLUMN id SET DEFAULT nextval('contacts_id_seq'::regclass);

ALTER TABLE ONLY custom_apps ALTER COLUMN id SET DEFAULT nextval('custom_apps_id_seq'::regclass);

ALTER TABLE ONLY dav_tokens ALTER COLUMN id SET DEFAULT nextval('dav_tokens_id_seq'::regclass);

ALTER TABLE ONLY devices ALTER COLUMN id SET DEFAULT nextval('devices_id_seq'::regclass);

ALTER TABLE ONLY dids ALTER COLUMN id SET DEFAULT nextval('dids_id_seq'::regclass);

ALTER TABLE ONLY groups ALTER COLUMN id SET DEFAULT nextval('groups_id_seq'::regclass);

ALTER TABLE ONLY holiday_sources ALTER COLUMN id SET DEFAULT nextval('holiday_sources_id_seq'::regclass);

ALTER TABLE ONLY ivrs ALTER COLUMN id SET DEFAULT nextval('ivrs_id_seq'::regclass);

ALTER TABLE ONLY moh_playlists ALTER COLUMN id SET DEFAULT nextval('moh_playlists_id_seq'::regclass);

ALTER TABLE ONLY outbound_routes ALTER COLUMN id SET DEFAULT nextval('outbound_routes_id_seq'::regclass);

ALTER TABLE ONLY provisioning_tokens ALTER COLUMN id SET DEFAULT nextval('provisioning_tokens_id_seq'::regclass);

ALTER TABLE ONLY sounds ALTER COLUMN id SET DEFAULT nextval('sounds_id_seq'::regclass);

ALTER TABLE ONLY targets ALTER COLUMN id SET DEFAULT nextval('targets_id_seq'::regclass);

ALTER TABLE ONLY time_profiles ALTER COLUMN id SET DEFAULT nextval('time_profiles_id_seq'::regclass);

ALTER TABLE ONLY time_rules ALTER COLUMN id SET DEFAULT nextval('time_rules_id_seq'::regclass);

ALTER TABLE ONLY trunk_inbound_rules ALTER COLUMN id SET DEFAULT nextval('trunk_inbound_rules_id_seq'::regclass);

ALTER TABLE ONLY trunks ALTER COLUMN id SET DEFAULT nextval('trunks_id_seq'::regclass);

ALTER TABLE ONLY user_blocked_numbers ALTER COLUMN id SET DEFAULT nextval('user_blocked_numbers_id_seq'::regclass);

ALTER TABLE ONLY users ALTER COLUMN id SET DEFAULT nextval('users_id_seq'::regclass);

ALTER TABLE ONLY voicemails ALTER COLUMN id SET DEFAULT nextval('voicemails_id_seq'::regclass);

ALTER TABLE ONLY app_media
    ADD CONSTRAINT app_media_pkey PRIMARY KEY (app_id, sound_id);

ALTER TABLE ONLY app_members
    ADD CONSTRAINT app_members_pkey PRIMARY KEY (app_id, user_id);

ALTER TABLE ONLY apps
    ADD CONSTRAINT apps_pkey PRIMARY KEY (id);

ALTER TABLE ONLY audit_log
    ADD CONSTRAINT audit_log_pkey PRIMARY KEY (id);

ALTER TABLE ONLY blocked_numbers
    ADD CONSTRAINT blocked_numbers_pattern_key UNIQUE (pattern);

ALTER TABLE ONLY blocked_numbers
    ADD CONSTRAINT blocked_numbers_pkey PRIMARY KEY (id);

ALTER TABLE ONLY bot_group_channels
    ADD CONSTRAINT bot_group_channels_pkey PRIMARY KEY (platform, channel, group_id);

ALTER TABLE ONLY bot_link_codes
    ADD CONSTRAINT bot_link_codes_pkey PRIMARY KEY (code_hash);

ALTER TABLE ONLY bot_links
    ADD CONSTRAINT bot_links_pkey PRIMARY KEY (platform, chat_user);

ALTER TABLE ONLY bot_links
    ADD CONSTRAINT bot_links_platform_user_id_key UNIQUE (platform, user_id);

ALTER TABLE ONLY bot_user_prefs
    ADD CONSTRAINT bot_user_prefs_pkey PRIMARY KEY (user_id);

ALTER TABLE ONLY call_notes
    ADD CONSTRAINT call_notes_pkey PRIMARY KEY (id);

ALTER TABLE ONLY cdr
    ADD CONSTRAINT cdr_pkey PRIMARY KEY (id);

ALTER TABLE ONLY contacts
    ADD CONSTRAINT contacts_pkey PRIMARY KEY (id);

ALTER TABLE ONLY custom_apps
    ADD CONSTRAINT custom_apps_pkey PRIMARY KEY (id);

ALTER TABLE ONLY dav_tokens
    ADD CONSTRAINT dav_tokens_pkey PRIMARY KEY (id);

ALTER TABLE ONLY devices
    ADD CONSTRAINT devices_pkey PRIMARY KEY (id);

ALTER TABLE ONLY devices
    ADD CONSTRAINT devices_sip_username_key UNIQUE (sip_username);

ALTER TABLE ONLY dids
    ADD CONSTRAINT dids_e164_key UNIQUE (e164);

ALTER TABLE ONLY dids
    ADD CONSTRAINT dids_pkey PRIMARY KEY (id);

ALTER TABLE ONLY greetings
    ADD CONSTRAINT greetings_pkey PRIMARY KEY (owner_type, owner_id, kind);

ALTER TABLE ONLY group_members
    ADD CONSTRAINT group_members_pkey PRIMARY KEY (group_id, user_id);

ALTER TABLE ONLY groups
    ADD CONSTRAINT groups_pkey PRIMARY KEY (id);

ALTER TABLE ONLY holiday_sources
    ADD CONSTRAINT holiday_sources_pkey PRIMARY KEY (id);

ALTER TABLE ONLY holidays
    ADD CONSTRAINT holidays_pkey PRIMARY KEY (region, date);

ALTER TABLE ONLY inbound_rules
    ADD CONSTRAINT inbound_rules_pkey PRIMARY KEY (did_id);

ALTER TABLE ONLY ivr_options
    ADD CONSTRAINT ivr_options_pkey PRIMARY KEY (ivr_id, digit);

ALTER TABLE ONLY ivrs
    ADD CONSTRAINT ivrs_pkey PRIMARY KEY (id);

ALTER TABLE ONLY local_logins
    ADD CONSTRAINT local_logins_login_key UNIQUE (login);

ALTER TABLE ONLY local_logins
    ADD CONSTRAINT local_logins_pkey PRIMARY KEY (user_id);

ALTER TABLE ONLY moh_media
    ADD CONSTRAINT moh_media_pkey PRIMARY KEY (playlist_id, sound_id);

ALTER TABLE ONLY moh_playlists
    ADD CONSTRAINT moh_playlists_pkey PRIMARY KEY (id);

ALTER TABLE ONLY number_holds
    ADD CONSTRAINT number_holds_pkey PRIMARY KEY (number);

ALTER TABLE ONLY number_policy
    ADD CONSTRAINT number_policy_pkey PRIMARY KEY (kind);

ALTER TABLE ONLY numbers
    ADD CONSTRAINT numbers_kind_ref_id_key UNIQUE (kind, ref_id);

ALTER TABLE ONLY numbers
    ADD CONSTRAINT numbers_pkey PRIMARY KEY (number);

ALTER TABLE ONLY outbound_routes
    ADD CONSTRAINT outbound_routes_pkey PRIMARY KEY (id);

ALTER TABLE ONLY profile_closures
    ADD CONSTRAINT profile_closures_pkey PRIMARY KEY (profile_id, date);

ALTER TABLE ONLY provisioning_tokens
    ADD CONSTRAINT provisioning_tokens_pkey PRIMARY KEY (id);

ALTER TABLE ONLY provisioning_tokens
    ADD CONSTRAINT provisioning_tokens_token_sha256_key UNIQUE (token_sha256);

ALTER TABLE ONLY route_classes
    ADD CONSTRAINT route_classes_pkey PRIMARY KEY (key);

ALTER TABLE ONLY settings
    ADD CONSTRAINT settings_pkey PRIMARY KEY (key);

ALTER TABLE ONLY sounds
    ADD CONSTRAINT sounds_pkey PRIMARY KEY (id);

ALTER TABLE ONLY targets
    ADD CONSTRAINT targets_pkey PRIMARY KEY (id);

ALTER TABLE ONLY time_profiles
    ADD CONSTRAINT time_profiles_pkey PRIMARY KEY (id);

ALTER TABLE ONLY time_rules
    ADD CONSTRAINT time_rules_pkey PRIMARY KEY (id);

ALTER TABLE ONLY trunk_inbound_rules
    ADD CONSTRAINT trunk_inbound_rules_pkey PRIMARY KEY (id);

ALTER TABLE ONLY trunks
    ADD CONSTRAINT trunks_name_key UNIQUE (name);

ALTER TABLE ONLY trunks
    ADD CONSTRAINT trunks_pkey PRIMARY KEY (id);

ALTER TABLE ONLY user_blocked_numbers
    ADD CONSTRAINT user_blocked_numbers_pkey PRIMARY KEY (id);

ALTER TABLE ONLY user_blocked_numbers
    ADD CONSTRAINT user_blocked_numbers_user_id_e164_key UNIQUE (user_id, e164);

ALTER TABLE ONLY users
    ADD CONSTRAINT users_kc_subject_key UNIQUE (kc_subject);

ALTER TABLE ONLY users
    ADD CONSTRAINT users_pkey PRIMARY KEY (id);

ALTER TABLE ONLY voicemails
    ADD CONSTRAINT voicemails_pkey PRIMARY KEY (id);

ALTER TABLE ONLY voicemails
    ADD CONSTRAINT voicemails_uuid_key UNIQUE (uuid);

-- ================================================================================================
-- Indexes
-- ================================================================================================

CREATE UNIQUE INDEX apps_number_live ON apps USING btree (number) WHERE (deleted_at IS NULL);

CREATE INDEX bot_group_channels_group ON bot_group_channels USING btree (group_id);

CREATE INDEX bot_link_codes_user ON bot_link_codes USING btree (user_id);

CREATE INDEX call_notes_e164 ON call_notes USING btree (e164);

CREATE INDEX call_notes_user ON call_notes USING btree (user_id);

CREATE INDEX cdr_calldate ON cdr USING btree (calldate);

CREATE INDEX cdr_channel_dev ON cdr USING btree (split_part(channel, '-'::text, 1));

CREATE INDEX cdr_dstchannel_dev ON cdr USING btree (split_part(dstchannel, '-'::text, 1));

CREATE INDEX cdr_linkedid ON cdr USING btree (linkedid);

CREATE UNIQUE INDEX contacts_dav_name ON contacts USING btree (user_id, dav_name);

CREATE INDEX contacts_user ON contacts USING btree (user_id);

CREATE UNIQUE INDEX custom_apps_number_live ON custom_apps USING btree (number) WHERE (deleted_at IS NULL);

CREATE INDEX dav_tokens_user ON dav_tokens USING btree (user_id);

CREATE UNIQUE INDEX devices_number_live ON devices USING btree (number) WHERE (deleted_at IS NULL);

CREATE UNIQUE INDEX groups_number_live ON groups USING btree (number) WHERE (deleted_at IS NULL);

CREATE UNIQUE INDEX ivrs_number_live ON ivrs USING btree (number) WHERE ((deleted_at IS NULL) AND (number IS NOT NULL));

CREATE UNIQUE INDEX moh_playlists_one_default ON moh_playlists USING btree ((true)) WHERE is_default;

CREATE UNIQUE INDEX moh_playlists_one_per_user ON moh_playlists USING btree (owner_user_id) WHERE (owner_user_id IS NOT NULL);

CREATE INDEX provisioning_tokens_device ON provisioning_tokens USING btree (device_id);

CREATE UNIQUE INDEX users_extension_live ON users USING btree (extension) WHERE (deleted_at IS NULL);

CREATE INDEX voicemails_owner ON voicemails USING btree (owner_type, owner_id) WHERE (deleted_at IS NULL);

-- ================================================================================================
-- Triggers
-- ================================================================================================

CREATE TRIGGER numbers_apps AFTER INSERT OR DELETE OR UPDATE OF number, deleted_at, owner_user_id ON apps FOR EACH ROW EXECUTE FUNCTION numbers_sync('number', 'app');

CREATE TRIGGER numbers_custom_apps AFTER INSERT OR DELETE OR UPDATE OF number, deleted_at ON custom_apps FOR EACH ROW EXECUTE FUNCTION numbers_sync('number', 'custom_app');

CREATE TRIGGER numbers_devices AFTER INSERT OR DELETE OR UPDATE OF number, deleted_at ON devices FOR EACH ROW EXECUTE FUNCTION numbers_sync('number', 'device');

CREATE TRIGGER numbers_groups AFTER INSERT OR DELETE OR UPDATE OF number, deleted_at ON groups FOR EACH ROW EXECUTE FUNCTION numbers_sync('number', 'group');

CREATE TRIGGER numbers_ivrs AFTER INSERT OR DELETE OR UPDATE OF number, deleted_at ON ivrs FOR EACH ROW EXECUTE FUNCTION numbers_sync('number', 'ivr');

CREATE TRIGGER numbers_users AFTER INSERT OR DELETE OR UPDATE OF extension, deleted_at ON users FOR EACH ROW EXECUTE FUNCTION numbers_sync('extension', 'person');

-- ================================================================================================
-- Foreign keys
-- ================================================================================================

ALTER TABLE ONLY app_media
    ADD CONSTRAINT app_media_app_id_fkey FOREIGN KEY (app_id) REFERENCES apps(id) ON DELETE CASCADE;

ALTER TABLE ONLY app_media
    ADD CONSTRAINT app_media_sound_id_fkey FOREIGN KEY (sound_id) REFERENCES sounds(id);

ALTER TABLE ONLY app_members
    ADD CONSTRAINT app_members_app_id_fkey FOREIGN KEY (app_id) REFERENCES apps(id) ON DELETE CASCADE;

ALTER TABLE ONLY app_members
    ADD CONSTRAINT app_members_user_id_fkey FOREIGN KEY (user_id) REFERENCES users(id);

ALTER TABLE ONLY apps
    ADD CONSTRAINT apps_owner_user_id_fkey FOREIGN KEY (owner_user_id) REFERENCES users(id);

ALTER TABLE ONLY bot_group_channels
    ADD CONSTRAINT bot_group_channels_group_id_fkey FOREIGN KEY (group_id) REFERENCES groups(id) ON DELETE CASCADE;

ALTER TABLE ONLY bot_group_channels
    ADD CONSTRAINT bot_group_channels_linked_by_fkey FOREIGN KEY (linked_by) REFERENCES users(id) ON DELETE SET NULL;

ALTER TABLE ONLY bot_link_codes
    ADD CONSTRAINT bot_link_codes_user_id_fkey FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE;

ALTER TABLE ONLY bot_links
    ADD CONSTRAINT bot_links_user_id_fkey FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE;

ALTER TABLE ONLY bot_user_prefs
    ADD CONSTRAINT bot_user_prefs_user_id_fkey FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE;

ALTER TABLE ONLY call_notes
    ADD CONSTRAINT call_notes_user_id_fkey FOREIGN KEY (user_id) REFERENCES users(id);

ALTER TABLE ONLY contacts
    ADD CONSTRAINT contacts_user_id_fkey FOREIGN KEY (user_id) REFERENCES users(id);

ALTER TABLE ONLY dav_tokens
    ADD CONSTRAINT dav_tokens_user_id_fkey FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE;

ALTER TABLE ONLY devices
    ADD CONSTRAINT devices_user_id_fkey FOREIGN KEY (user_id) REFERENCES users(id);

ALTER TABLE ONLY dids
    ADD CONSTRAINT dids_trunk_id_fkey FOREIGN KEY (trunk_id) REFERENCES trunks(id);

ALTER TABLE ONLY greetings
    ADD CONSTRAINT greetings_sound_id_fkey FOREIGN KEY (sound_id) REFERENCES sounds(id);

ALTER TABLE ONLY group_members
    ADD CONSTRAINT group_members_group_id_fkey FOREIGN KEY (group_id) REFERENCES groups(id);

ALTER TABLE ONLY group_members
    ADD CONSTRAINT group_members_user_id_fkey FOREIGN KEY (user_id) REFERENCES users(id);

ALTER TABLE ONLY groups
    ADD CONSTRAINT groups_announce_sound_id_fkey FOREIGN KEY (announce_sound_id) REFERENCES sounds(id);

ALTER TABLE ONLY groups
    ADD CONSTRAINT groups_moh_playlist_id_fkey FOREIGN KEY (moh_playlist_id) REFERENCES moh_playlists(id);

ALTER TABLE ONLY groups
    ADD CONSTRAINT groups_no_answer_target_fkey FOREIGN KEY (no_answer_target) REFERENCES targets(id);

ALTER TABLE ONLY holidays
    ADD CONSTRAINT holidays_source_id_fkey FOREIGN KEY (source_id) REFERENCES holiday_sources(id) ON DELETE CASCADE;

ALTER TABLE ONLY inbound_rules
    ADD CONSTRAINT inbound_rules_did_id_fkey FOREIGN KEY (did_id) REFERENCES dids(id);

ALTER TABLE ONLY inbound_rules
    ADD CONSTRAINT inbound_rules_target_closed_fkey FOREIGN KEY (target_closed) REFERENCES targets(id);

ALTER TABLE ONLY inbound_rules
    ADD CONSTRAINT inbound_rules_target_holiday_fkey FOREIGN KEY (target_holiday) REFERENCES targets(id);

ALTER TABLE ONLY inbound_rules
    ADD CONSTRAINT inbound_rules_target_open_fkey FOREIGN KEY (target_open) REFERENCES targets(id);

ALTER TABLE ONLY inbound_rules
    ADD CONSTRAINT inbound_rules_time_profile_id_fkey FOREIGN KEY (time_profile_id) REFERENCES time_profiles(id);

ALTER TABLE ONLY ivr_options
    ADD CONSTRAINT ivr_options_ivr_id_fkey FOREIGN KEY (ivr_id) REFERENCES ivrs(id);

ALTER TABLE ONLY ivr_options
    ADD CONSTRAINT ivr_options_target_id_fkey FOREIGN KEY (target_id) REFERENCES targets(id);

ALTER TABLE ONLY ivrs
    ADD CONSTRAINT ivrs_greeting_fkey FOREIGN KEY (greeting) REFERENCES sounds(id);

ALTER TABLE ONLY ivrs
    ADD CONSTRAINT ivrs_timeout_target_fkey FOREIGN KEY (timeout_target) REFERENCES targets(id);

ALTER TABLE ONLY local_logins
    ADD CONSTRAINT local_logins_user_id_fkey FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE;

ALTER TABLE ONLY moh_media
    ADD CONSTRAINT moh_media_playlist_id_fkey FOREIGN KEY (playlist_id) REFERENCES moh_playlists(id) ON DELETE CASCADE;

ALTER TABLE ONLY moh_media
    ADD CONSTRAINT moh_media_sound_id_fkey FOREIGN KEY (sound_id) REFERENCES sounds(id);

ALTER TABLE ONLY moh_playlists
    ADD CONSTRAINT moh_playlists_owner_user_id_fkey FOREIGN KEY (owner_user_id) REFERENCES users(id);

ALTER TABLE ONLY outbound_routes
    ADD CONSTRAINT outbound_routes_class_fk FOREIGN KEY (route_class) REFERENCES route_classes(key) ON UPDATE CASCADE;

ALTER TABLE ONLY outbound_routes
    ADD CONSTRAINT outbound_routes_clip_did_id_fkey FOREIGN KEY (clip_did_id) REFERENCES dids(id);

ALTER TABLE ONLY outbound_routes
    ADD CONSTRAINT outbound_routes_trunk_id_fkey FOREIGN KEY (trunk_id) REFERENCES trunks(id);

ALTER TABLE ONLY profile_closures
    ADD CONSTRAINT profile_closures_profile_id_fkey FOREIGN KEY (profile_id) REFERENCES time_profiles(id) ON DELETE CASCADE;

ALTER TABLE ONLY provisioning_tokens
    ADD CONSTRAINT provisioning_tokens_device_id_fkey FOREIGN KEY (device_id) REFERENCES devices(id);

ALTER TABLE ONLY sounds
    ADD CONSTRAINT sounds_owner_user_id_fkey FOREIGN KEY (owner_user_id) REFERENCES users(id);

ALTER TABLE ONLY sounds
    ADD CONSTRAINT sounds_uploaded_by_fkey FOREIGN KEY (uploaded_by) REFERENCES users(id);

ALTER TABLE ONLY targets
    ADD CONSTRAINT targets_app_id_fkey FOREIGN KEY (app_id) REFERENCES apps(id);

ALTER TABLE ONLY targets
    ADD CONSTRAINT targets_group_id_fkey FOREIGN KEY (group_id) REFERENCES groups(id);

ALTER TABLE ONLY targets
    ADD CONSTRAINT targets_ivr_id_fkey FOREIGN KEY (ivr_id) REFERENCES ivrs(id);

ALTER TABLE ONLY targets
    ADD CONSTRAINT targets_owner_user_id_fkey FOREIGN KEY (owner_user_id) REFERENCES users(id);

ALTER TABLE ONLY targets
    ADD CONSTRAINT targets_sound_id_fkey FOREIGN KEY (sound_id) REFERENCES sounds(id);

ALTER TABLE ONLY targets
    ADD CONSTRAINT targets_user_id_fkey FOREIGN KEY (user_id) REFERENCES users(id);

ALTER TABLE ONLY time_rules
    ADD CONSTRAINT time_rules_profile_id_fkey FOREIGN KEY (profile_id) REFERENCES time_profiles(id);

ALTER TABLE ONLY trunk_inbound_rules
    ADD CONSTRAINT trunk_inbound_rules_target_id_fkey FOREIGN KEY (target_id) REFERENCES targets(id);

ALTER TABLE ONLY trunk_inbound_rules
    ADD CONSTRAINT trunk_inbound_rules_trunk_id_fkey FOREIGN KEY (trunk_id) REFERENCES trunks(id) ON DELETE CASCADE;

ALTER TABLE ONLY user_blocked_numbers
    ADD CONSTRAINT user_blocked_numbers_user_id_fkey FOREIGN KEY (user_id) REFERENCES users(id);

ALTER TABLE ONLY users
    ADD CONSTRAINT users_forward_immediate_fkey FOREIGN KEY (forward_immediate) REFERENCES targets(id);

ALTER TABLE ONLY users
    ADD CONSTRAINT users_no_answer_target_fkey FOREIGN KEY (no_answer_target) REFERENCES targets(id);

ALTER TABLE ONLY users
    ADD CONSTRAINT users_outbound_did_id_fkey FOREIGN KEY (outbound_did_id) REFERENCES dids(id);

ALTER TABLE ONLY voicemails
    ADD CONSTRAINT voicemails_did_id_fkey FOREIGN KEY (did_id) REFERENCES dids(id);

ALTER TABLE ONLY voicemails
    ADD CONSTRAINT voicemails_heard_by_fkey FOREIGN KEY (heard_by) REFERENCES users(id);

-- ================================================================================================
-- Permissions
-- ================================================================================================

GRANT USAGE ON SCHEMA public TO pbxweb;

REVOKE ALL ON FUNCTION caller_blocked(num text) FROM PUBLIC;
GRANT ALL ON FUNCTION caller_blocked(num text) TO pbxweb;
GRANT ALL ON FUNCTION caller_blocked(num text) TO asterisk;

GRANT ALL ON FUNCTION caller_e164(num text) TO pbxweb;
GRANT ALL ON FUNCTION caller_e164(num text) TO asterisk;

REVOKE ALL ON FUNCTION contact_name(ext text, num text) FROM PUBLIC;
GRANT ALL ON FUNCTION contact_name(ext text, num text) TO pbxweb;
GRANT ALL ON FUNCTION contact_name(ext text, num text) TO asterisk;

GRANT ALL ON FUNCTION may_dial(uid integer, n text) TO pbxweb;
GRANT ALL ON FUNCTION may_dial(uid integer, n text) TO asterisk;

GRANT ALL ON FUNCTION numbers_sync() TO pbxweb;

GRANT ALL ON FUNCTION route_for(n text) TO pbxweb;
GRANT ALL ON FUNCTION route_for(n text) TO asterisk;

GRANT ALL ON FUNCTION time_profile_state(p integer) TO asterisk;
GRANT ALL ON FUNCTION time_profile_state(p integer) TO pbxweb;

REVOKE ALL ON FUNCTION user_caller_blocked(ext text, num text) FROM PUBLIC;
GRANT ALL ON FUNCTION user_caller_blocked(ext text, num text) TO pbxweb;
GRANT ALL ON FUNCTION user_caller_blocked(ext text, num text) TO asterisk;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE devices TO pbxweb;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE users TO pbxweb;

GRANT SELECT(id) ON TABLE users TO asterisk;

GRANT SELECT(kind) ON TABLE users TO asterisk;

GRANT SELECT(extension) ON TABLE users TO asterisk;

GRANT SELECT(display_name) ON TABLE users TO asterisk;

GRANT SELECT(enabled) ON TABLE users TO asterisk;

GRANT SELECT(phone_allowed) ON TABLE users TO asterisk;

GRANT SELECT(route_classes) ON TABLE users TO asterisk;

GRANT SELECT(outbound_did_id) ON TABLE users TO asterisk;

GRANT SELECT(language) ON TABLE users TO asterisk;

GRANT SELECT(dnd) ON TABLE users TO asterisk;

GRANT SELECT(ring_timeout_s) ON TABLE users TO asterisk;

GRANT SELECT(pickup_group) ON TABLE users TO asterisk;

GRANT SELECT(deleted_at) ON TABLE users TO asterisk;

GRANT SELECT(forward_immediate) ON TABLE users TO asterisk;

GRANT SELECT(no_answer_target) ON TABLE users TO asterisk;

GRANT SELECT(in_phonebook) ON TABLE users TO asterisk;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE active_devices TO pbxweb;
GRANT SELECT ON TABLE active_devices TO asterisk;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE app_media TO pbxweb;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE apps TO pbxweb;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE settings TO pbxweb;
GRANT SELECT ON TABLE settings TO asterisk;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE app_dialplan TO pbxweb;
GRANT SELECT ON TABLE app_dialplan TO asterisk;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE app_members TO pbxweb;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE app_group TO pbxweb;
GRANT SELECT ON TABLE app_group TO asterisk;

GRANT ALL ON SEQUENCE apps_id_seq TO pbxweb;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE audit_log TO pbxweb;

GRANT ALL ON SEQUENCE audit_log_id_seq TO pbxweb;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE blocked_numbers TO pbxweb;

GRANT ALL ON SEQUENCE blocked_numbers_id_seq TO pbxweb;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE bot_group_channels TO pbxweb;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE bot_link_codes TO pbxweb;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE bot_links TO pbxweb;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE bot_user_prefs TO pbxweb;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE call_notes TO pbxweb;

GRANT ALL ON SEQUENCE call_notes_id_seq TO pbxweb;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE cdr TO pbxweb;
GRANT INSERT ON TABLE cdr TO asterisk;

GRANT USAGE ON SEQUENCE cdr_id_seq TO asterisk;
GRANT ALL ON SEQUENCE cdr_id_seq TO pbxweb;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE contacts TO pbxweb;

GRANT ALL ON SEQUENCE contacts_id_seq TO pbxweb;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE custom_apps TO pbxweb;
GRANT SELECT ON TABLE custom_apps TO asterisk;

GRANT ALL ON SEQUENCE custom_apps_id_seq TO pbxweb;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE dav_tokens TO pbxweb;

GRANT ALL ON SEQUENCE dav_tokens_id_seq TO pbxweb;

GRANT ALL ON SEQUENCE devices_id_seq TO pbxweb;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE dids TO pbxweb;
GRANT SELECT ON TABLE dids TO asterisk;

GRANT ALL ON SEQUENCE dids_id_seq TO pbxweb;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE greetings TO pbxweb;
GRANT SELECT ON TABLE greetings TO asterisk;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE group_members TO pbxweb;
GRANT SELECT ON TABLE group_members TO asterisk;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE group_ringing_members TO pbxweb;
GRANT SELECT ON TABLE group_ringing_members TO asterisk;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE groups TO pbxweb;
GRANT SELECT ON TABLE groups TO asterisk;

GRANT ALL ON SEQUENCE groups_id_seq TO pbxweb;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE holiday_sources TO pbxweb;

GRANT ALL ON SEQUENCE holiday_sources_id_seq TO pbxweb;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE holidays TO pbxweb;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE inbound_rules TO pbxweb;
GRANT SELECT ON TABLE inbound_rules TO asterisk;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE ivr_options TO pbxweb;
GRANT SELECT ON TABLE ivr_options TO asterisk;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE ivrs TO pbxweb;
GRANT SELECT ON TABLE ivrs TO asterisk;

GRANT ALL ON SEQUENCE ivrs_id_seq TO pbxweb;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE local_logins TO pbxweb;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE moh_media TO pbxweb;
GRANT SELECT ON TABLE moh_media TO asterisk;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE moh_playlists TO pbxweb;
GRANT SELECT ON TABLE moh_playlists TO asterisk;

GRANT ALL ON SEQUENCE moh_playlists_id_seq TO pbxweb;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE musiconhold TO pbxweb;
GRANT SELECT ON TABLE musiconhold TO asterisk;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE number_holds TO pbxweb;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE number_policy TO pbxweb;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE numbers TO pbxweb;
GRANT SELECT ON TABLE numbers TO asterisk;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE outbound_routes TO pbxweb;
GRANT SELECT ON TABLE outbound_routes TO asterisk;

GRANT ALL ON SEQUENCE outbound_routes_id_seq TO pbxweb;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE profile_closures TO pbxweb;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE provisioning_tokens TO pbxweb;

GRANT ALL ON SEQUENCE provisioning_tokens_id_seq TO pbxweb;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE ps_aors TO pbxweb;
GRANT SELECT ON TABLE ps_aors TO asterisk;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE ps_auths TO pbxweb;
GRANT SELECT ON TABLE ps_auths TO asterisk;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE ps_endpoints TO pbxweb;
GRANT SELECT ON TABLE ps_endpoints TO asterisk;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE queue_members_rt TO pbxweb;
GRANT SELECT ON TABLE queue_members_rt TO asterisk;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE queue_rt TO pbxweb;
GRANT SELECT ON TABLE queue_rt TO asterisk;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE route_classes TO pbxweb;
GRANT SELECT ON TABLE route_classes TO asterisk;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE sounds TO pbxweb;
GRANT SELECT ON TABLE sounds TO asterisk;

GRANT ALL ON SEQUENCE sounds_id_seq TO pbxweb;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE targets TO pbxweb;
GRANT SELECT ON TABLE targets TO asterisk;

GRANT ALL ON SEQUENCE targets_id_seq TO pbxweb;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE time_profiles TO pbxweb;

GRANT ALL ON SEQUENCE time_profiles_id_seq TO pbxweb;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE time_rules TO pbxweb;

GRANT ALL ON SEQUENCE time_rules_id_seq TO pbxweb;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE trunk_inbound_rules TO pbxweb;
GRANT SELECT ON TABLE trunk_inbound_rules TO asterisk;

GRANT ALL ON SEQUENCE trunk_inbound_rules_id_seq TO pbxweb;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE trunks TO pbxweb;

GRANT SELECT(id) ON TABLE trunks TO asterisk;

GRANT SELECT(name) ON TABLE trunks TO asterisk;

GRANT SELECT(kind) ON TABLE trunks TO asterisk;

GRANT SELECT(asterisk_ref) ON TABLE trunks TO asterisk;

GRANT SELECT(enabled) ON TABLE trunks TO asterisk;

GRANT SELECT(inbound_mode) ON TABLE trunks TO asterisk;

GRANT SELECT(max_channels) ON TABLE trunks TO asterisk;

GRANT SELECT(managed) ON TABLE trunks TO asterisk;

GRANT SELECT(host) ON TABLE trunks TO asterisk;

GRANT SELECT(port) ON TABLE trunks TO asterisk;

GRANT SELECT(encryption) ON TABLE trunks TO asterisk;

GRANT SELECT(iax_trunk) ON TABLE trunks TO asterisk;

GRANT SELECT(calltoken) ON TABLE trunks TO asterisk;

GRANT SELECT(site_prefix) ON TABLE trunks TO asterisk;

GRANT SELECT(route_classes) ON TABLE trunks TO asterisk;

GRANT SELECT(deleted_at) ON TABLE trunks TO asterisk;

GRANT SELECT(sip_transport) ON TABLE trunks TO asterisk;

GRANT ALL ON SEQUENCE trunks_id_seq TO pbxweb;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE user_blocked_numbers TO pbxweb;

GRANT ALL ON SEQUENCE user_blocked_numbers_id_seq TO pbxweb;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE user_effective_classes TO pbxweb;
GRANT SELECT ON TABLE user_effective_classes TO asterisk;

GRANT ALL ON SEQUENCE users_id_seq TO pbxweb;

GRANT SELECT,INSERT,DELETE,UPDATE ON TABLE voicemails TO pbxweb;

GRANT ALL ON SEQUENCE voicemails_id_seq TO pbxweb;

ALTER DEFAULT PRIVILEGES FOR ROLE pbx IN SCHEMA public GRANT ALL ON SEQUENCES TO pbxweb;

ALTER DEFAULT PRIVILEGES FOR ROLE pbx IN SCHEMA public GRANT ALL ON FUNCTIONS TO pbxweb;

ALTER DEFAULT PRIVILEGES FOR ROLE pbx IN SCHEMA public GRANT SELECT,INSERT,DELETE,UPDATE ON TABLES TO pbxweb;

-- ================================================================================================
-- Initial data
-- ================================================================================================

-- Permission classes for dial rules: internal and emergency are built in; the others are a starting
-- point for German sites (renamed, changed or deleted in the web interface).
INSERT INTO route_classes (key, name, description, default_on, builtin) VALUES
    ('internal', 'Intern', 'Interne Nummern und IAX-Verbund', true, true),
    ('emergency', 'Notruf', 'Notruf und Behördenrufnummern, für alle erlaubt', false, true),
    ('national', 'Festnetz DE', 'Deutsche Festnetznummern', true, false),
    ('mobile', 'Mobil DE', 'Deutsche Mobilfunknummern', true, false),
    ('international', 'Ausland', 'Alle internationalen Ziele', false, false),
    ('eventphone', 'Eventphone', 'Eventphone-Netz (01999)', false, false);
