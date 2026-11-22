-- SPDX-License-Identifier: GPL-3.0-or-later
-- Trunk selection by the caller's number: a dial rule lists several trunks in order instead of one,
-- and the trunk that owns the caller's outbound number is tried first. Trunks can be marked as
-- showing only their own numbers (the provider replaces any other caller ID).
-- Design: docs/architecture.md (Outbound: route classes and dial rules).

ALTER TABLE trunks ADD COLUMN clip_own_only boolean NOT NULL DEFAULT false;

COMMENT ON COLUMN trunks.clip_own_only IS
    'The provider shows only this trunk''s own numbers (dids.trunk_id) as caller ID; another number is not sent.';

-- The dialplan gets a rule's trunks as one string ("ref/kind/max/clip/clip trunk|…", func_odbc
-- ROUTE): the separators must never occur in a trunk's Asterisk name. patchbay-web generates
-- "sip-<id>" and "site<prefix>"; rows added by hand must be renamed first.
DO $$
DECLARE
    bad text;
BEGIN
    SELECT string_agg(format('%s (id %s)', asterisk_ref, id), ', ') INTO bad FROM trunks WHERE asterisk_ref !~ '^[^|/,[:space:]]+$';
    IF bad IS NOT NULL THEN
        RAISE EXCEPTION 'Trunk Asterisk names with "|", "/", "," or spaces: %. Rename them (and their Asterisk configuration), then restart.', bad;
    END IF;
END $$;

ALTER TABLE trunks ADD CONSTRAINT trunks_asterisk_ref_check CHECK (asterisk_ref ~ '^[^|/,[:space:]]+$');

-- Trunks of a dial rule, tried in position order (route_trunk_order() puts the caller's trunk first).
CREATE TABLE route_trunks (
    route_id integer NOT NULL REFERENCES outbound_routes(id) ON DELETE CASCADE,
    position smallint NOT NULL CHECK (position >= 1),
    trunk_id integer NOT NULL REFERENCES trunks(id),
    PRIMARY KEY (route_id, position),
    UNIQUE (route_id, trunk_id)
);

-- "trunk still used by a dial rule", trunk page
CREATE INDEX route_trunks_trunk_id_idx ON route_trunks (trunk_id);

INSERT INTO route_trunks (route_id, position, trunk_id)
SELECT id, 1, trunk_id FROM outbound_routes WHERE trunk_id IS NOT NULL;

-- the CHECK "action route needs a trunk" goes with the column; patchbay-web checks it on save, and a
-- rule without a usable trunk denies (dialplan, may_dial)
ALTER TABLE outbound_routes DROP COLUMN trunk_id;

-- The rule a (normalised) number takes. Its trunks: route_trunk_order().
DROP FUNCTION route_for(text);

CREATE FUNCTION route_for(n text) RETURNS TABLE(route_id integer, action text, strip_digits smallint, prepend text, route_class text, clip_mode text, clip_e164 text)
    LANGUAGE sql STABLE
    AS $$
    SELECT r.id, r.action::text, r.strip_digits, r.prepend, r.route_class, r.clip_mode::text, COALESCE(d.e164, '')
    FROM outbound_routes r
    LEFT JOIN dids d ON d.id = r.clip_did_id
    WHERE n LIKE r.match_prefix || '%' AND length(n) >= r.min_len AND (r.max_len IS NULL OR length(n) <= r.max_len)
    ORDER BY r.priority, length(r.match_prefix) DESC
    LIMIT 1
$$;

-- The trunks one call tries, in order, with the caller ID per trunk. caller_did = the caller's
-- outbound number (E.164, users.outbound_did_id; '' = none).
--   order:     the rule's order (only enabled, not deleted trunks); the trunk that owns caller_did
--              moves to the front (preferred) when the rule shows the person's number (clip_mode 'user');
--              emergency calls keep the rule's order
--   caller ID: clip_mode 'user' = caller_did, else the rule's number; 'route' = the rule's number.
--              A trunk with clip_own_only gets only a number of its own: else the rule's number if it
--              belongs to that trunk, else '' (the provider shows the trunk's own number).
--   clip_trunk: Asterisk name of the trunk that owns clip_e164 ('' if none): the control centre
--              calls an emergency caller back on the number it was shown.
-- Shared by the dialplan (func_odbc ROUTE), may_dial() and patchbay-web (route tester).
CREATE FUNCTION route_trunk_order(route integer, caller_did text)
    RETURNS TABLE(attempt integer, trunk_id integer, trunk text, trunk_name text, trunk_kind text, max_channels integer, clip_e164 text, preferred boolean, clip_trunk text)
    LANGUAGE sql STABLE
    AS $$
    WITH r AS (
        SELECT o.id, o.route_class, o.clip_mode, d.e164 AS route_did, d.trunk_id AS route_did_trunk,
               CASE WHEN o.clip_mode = 'user' AND COALESCE(caller_did, '') <> '' THEN caller_did ELSE COALESCE(d.e164, '') END AS clip
        FROM outbound_routes o LEFT JOIN dids d ON d.id = o.clip_did_id
        WHERE o.id = route
    ), l AS (
        SELECT rt.position, t.id, t.asterisk_ref, t.name, t.kind, t.max_channels, t.clip_own_only,
               r.route_class <> 'emergency' AND r.clip_mode = 'user' AND EXISTS (SELECT 1 FROM dids c WHERE c.e164 = caller_did AND c.trunk_id = t.id) AS preferred,
               CASE
                   WHEN NOT t.clip_own_only THEN r.clip
                   WHEN EXISTS (SELECT 1 FROM dids o WHERE o.e164 = r.clip AND o.trunk_id = t.id) THEN r.clip
                   WHEN r.route_did_trunk = t.id THEN r.route_did
                   ELSE ''
               END AS clip
        FROM r
        JOIN route_trunks rt ON rt.route_id = r.id
        JOIN trunks t ON t.id = rt.trunk_id AND t.enabled AND t.deleted_at IS NULL
    )
    SELECT (row_number() OVER (ORDER BY l.preferred DESC, l.position))::integer, l.id, l.asterisk_ref, l.name, l.kind::text,
           COALESCE(l.max_channels, 0)::integer, l.clip, l.preferred,
           COALESCE((SELECT ct.asterisk_ref FROM dids cd JOIN trunks ct ON ct.id = cd.trunk_id WHERE cd.e164 = l.clip), '')
    FROM l
    ORDER BY 1
$$;

CREATE OR REPLACE FUNCTION may_dial(uid integer, n text) RETURNS boolean
    LANGUAGE sql STABLE
    AS $$
    SELECT COALESCE((
        SELECT r.action = 'route' AND EXISTS (SELECT 1 FROM route_trunk_order(r.route_id, '')) AND r.route_class = ANY (e.classes)
        FROM route_for(n) r, user_effective_classes e JOIN users u ON u.id = e.user_id
        WHERE e.user_id = uid AND u.enabled AND u.phone_allowed AND u.deleted_at IS NULL
    ), false)
$$;

-- route_for() and route_trunk_order() run with the caller's rights (no SECURITY DEFINER): asterisk
-- reads the routing tables itself, column-level on trunks as before.
GRANT ALL ON FUNCTION route_for(n text) TO pbxweb;
GRANT ALL ON FUNCTION route_for(n text) TO asterisk;
GRANT ALL ON FUNCTION route_trunk_order(route integer, caller_did text) TO pbxweb;
GRANT ALL ON FUNCTION route_trunk_order(route integer, caller_did text) TO asterisk;
GRANT SELECT, INSERT, DELETE, UPDATE ON TABLE route_trunks TO pbxweb;
GRANT SELECT ON TABLE route_trunks TO asterisk;
GRANT SELECT (clip_own_only) ON TABLE trunks TO asterisk;
