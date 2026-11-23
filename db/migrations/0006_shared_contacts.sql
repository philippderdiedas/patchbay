-- Shared phone book (PBX_SHARED_CONTACTS=off|read|write): contacts without an owner (user_id NULL),
-- visible to everyone with phone access, edited by everyone (write) or only by admins (read).
-- Who created and last changed an entry is kept; deleting moves it to a bin for 30 days
-- (deleted_at, restored by admins, purged by housekeeping). Personal contacts are unchanged.

ALTER TABLE contacts ALTER COLUMN user_id DROP NOT NULL;
ALTER TABLE contacts
    ADD COLUMN created_by integer REFERENCES users(id) ON DELETE SET NULL,
    ADD COLUMN updated_by integer REFERENCES users(id) ON DELETE SET NULL,
    ADD COLUMN deleted_at timestamp with time zone,
    ADD COLUMN deleted_by integer REFERENCES users(id) ON DELETE SET NULL;
-- only shared entries go to the bin; personal ones are deleted right away
ALTER TABLE contacts ADD CONSTRAINT contacts_bin_shared_only CHECK (deleted_at IS NULL OR user_id IS NULL);

-- CardDAV resource names are unique per book; the existing (user_id, dav_name) index does not cover
-- user_id NULL (NULLs are distinct)
CREATE UNIQUE INDEX contacts_shared_dav_name ON contacts (dav_name) WHERE user_id IS NULL AND deleted_at IS NULL;
CREATE INDEX contacts_shared_phone ON contacts (phone_e164) WHERE user_id IS NULL AND deleted_at IS NULL;
CREATE INDEX contacts_shared_mobile ON contacts (mobile_e164) WHERE user_id IS NULL AND deleted_at IS NULL;

-- Caller names: the called person's own contacts first, then the shared phone book (setting
-- shared_contacts, written by patchbay-web from PBX_SHARED_CONTACTS).
CREATE OR REPLACE FUNCTION contact_name(ext text, num text) RETURNS text
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
    SELECT btrim(regexp_replace(name, '[[:cntrl:][:space:]",\\]+', ' ', 'g')) FROM (
        SELECT k.name, 0 AS prio, k.id
        FROM contacts k JOIN users u ON u.id = k.user_id, e
        WHERE u.extension::text = ext AND u.deleted_at IS NULL AND length(e.raw) > 4
          AND (k.phone = e.raw OR k.mobile = e.raw OR k.phone_e164 = e.e164 OR k.mobile_e164 = e.e164)
        UNION ALL
        SELECT k.name, 1 AS prio, k.id
        FROM contacts k, e
        WHERE k.user_id IS NULL AND k.deleted_at IS NULL AND length(e.raw) > 4
          AND COALESCE((SELECT value FROM settings WHERE key = 'shared_contacts'), 'off') <> 'off'
          AND (k.phone = e.raw OR k.mobile = e.raw OR k.phone_e164 = e.e164 OR k.mobile_e164 = e.e164)
    ) m
    ORDER BY prio, id
    LIMIT 1
$$;
