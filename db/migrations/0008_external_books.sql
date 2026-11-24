-- External phone books (PBX_EXTERNAL_BOOKS, extbooks.rs): read-only copies of CardDAV address books of
-- other systems (ERP/CRM), loaded periodically by patchbay-web. Caller names fall back to them after
-- the person's own contacts and the shared phone book.

CREATE TABLE external_contacts (
    book text NOT NULL,
    res text NOT NULL,
    etag text,
    name text NOT NULL,
    organization text,
    phone text,
    phone_e164 text,
    mobile text,
    mobile_e164 text,
    email text,
    -- link to the record in the other system (vCard URL, http/https only)
    url text,
    vcard text NOT NULL,
    PRIMARY KEY (book, res)
);
CREATE INDEX external_contacts_phone ON external_contacts (phone_e164);
CREATE INDEX external_contacts_mobile ON external_contacts (mobile_e164);

CREATE TABLE external_books (
    book text PRIMARY KEY,
    ctag text,
    synced_at timestamp with time zone,
    checked_at timestamp with time zone,
    error text,
    cards integer
);

GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE external_contacts TO pbxweb;
GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE external_books TO pbxweb;

-- Caller names: own contacts, then the shared phone book (settings.shared_contacts), then the external
-- phone books.
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
        UNION ALL
        SELECT x.name, 2 AS prio, 0 AS id
        FROM external_contacts x, e
        WHERE length(e.raw) > 4
          AND (x.phone = e.raw OR x.mobile = e.raw OR x.phone_e164 = e.e164 OR x.mobile_e164 = e.e164)
    ) m
    ORDER BY prio, id
    LIMIT 1
$$;
