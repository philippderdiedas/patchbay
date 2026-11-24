-- Caller names with the company (PBX_CALLER_NAME_FORMAT=name_org, setting caller_name_format written by
-- patchbay-web): "Hauke Oldsen (Cedgetec GmbH)" on the phones; not when the contact is the company
-- itself or has none. Phones cut long names at the end, so the person stays visible.
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
    SELECT btrim(regexp_replace(
               CASE WHEN COALESCE((SELECT value FROM settings WHERE key = 'caller_name_format'), 'name') = 'name_org'
                         AND btrim(regexp_replace(COALESCE(org, ''), '[[:cntrl:][:space:]",\\]+', ' ', 'g')) <> ''
                         AND lower(btrim(org)) <> lower(btrim(name))
                    THEN name || ' (' || org || ')' ELSE name END,
               '[[:cntrl:][:space:]",\\]+', ' ', 'g')) FROM (
        SELECT k.name, k.organization AS org, 0 AS prio, k.id, '' AS ext
        FROM contacts k JOIN users u ON u.id = k.user_id, e
        WHERE u.extension::text = ext AND u.deleted_at IS NULL AND length(e.raw) > 4
          AND (k.phone = e.raw OR k.mobile = e.raw OR k.phone_e164 = e.e164 OR k.mobile_e164 = e.e164)
        UNION ALL
        SELECT k.name, k.organization, 1 AS prio, k.id, ''
        FROM contacts k, e
        WHERE k.user_id IS NULL AND k.deleted_at IS NULL AND length(e.raw) > 4
          AND COALESCE((SELECT value FROM settings WHERE key = 'shared_contacts'), 'off') <> 'off'
          AND (k.phone = e.raw OR k.mobile = e.raw OR k.phone_e164 = e.e164 OR k.mobile_e164 = e.e164)
        UNION ALL
        -- external books: by the full number only (a short code there must not name a colleague)
        SELECT x.name, x.organization, 2 AS prio, 0, x.book || '/' || x.res
        FROM external_contacts x, e
        WHERE length(e.raw) > 4 AND e.e164 IS NOT NULL AND (x.phone_e164 = e.e164 OR x.mobile_e164 = e.e164)
    ) m
    ORDER BY prio, id, ext
    LIMIT 1
$$;
