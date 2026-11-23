-- Mobile phone as a device: rings in parallel with the person's SIP devices (sub-user), dialled
-- out over the trunks with the person's own dialling permissions (ring-mobile -> sub-outbound).
-- It never registers: no SIP account (sip_username/ha1 are random placeholders), not in
-- active_devices and therefore not in the PJSIP views, hints, pickup or click-to-call.

ALTER TYPE device_type ADD VALUE IF NOT EXISTS 'mobile';

-- The new enum value must not be used in this transaction, hence type::text everywhere below.
ALTER TABLE devices ADD COLUMN mobile_e164 text
    CONSTRAINT devices_mobile_e164_check CHECK (mobile_e164 ~ '^\+[1-9][0-9]{6,14}$');
ALTER TABLE devices ADD CONSTRAINT devices_mobile_check
    CHECK ((type::text = 'mobile') = (mobile_e164 IS NOT NULL) AND (type::text <> 'mobile' OR number IS NULL));

-- SIP devices only (unchanged columns; dependent views keep working)
CREATE OR REPLACE VIEW active_devices AS
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
  WHERE (d.enabled AND (d.deleted_at IS NULL) AND u.enabled AND u.phone_allowed AND (u.deleted_at IS NULL)
         AND (d.type::text <> 'mobile'));

-- Mobile phones that ring for calls to their person (ringing switched on)
CREATE VIEW mobile_legs AS
 SELECT d.id,
    d.user_id,
    u.extension,
    d.mobile_e164,
    d.ring_delay_s,
    d.ring_order
   FROM (devices d
     JOIN users u ON ((u.id = d.user_id)))
  WHERE (d.enabled AND d.ring AND (d.deleted_at IS NULL) AND u.enabled AND u.phone_allowed AND (u.deleted_at IS NULL)
         AND (d.type::text = 'mobile'));

GRANT SELECT ON TABLE mobile_legs TO asterisk;
GRANT SELECT ON TABLE mobile_legs TO pbxweb;
