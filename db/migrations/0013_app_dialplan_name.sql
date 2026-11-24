-- The app's name for the dialplan: an app that answers itself (conference, music, announcement,
-- echo) sets it as the connected line, so the caller's phone and the status page show the app
-- instead of whoever transferred the call there. Without commas, quotes, backslashes and control
-- characters (func_odbc returns comma-separated fields).
CREATE OR REPLACE VIEW app_dialplan AS
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
    COALESCE((a.config ->> 'target'::text), ''::text) AS target,
    btrim(regexp_replace(a.name, '[[:cntrl:][:space:]",\\]+', ' ', 'g')) AS name
   FROM (apps a
     LEFT JOIN users u ON ((u.id = a.owner_user_id)))
  WHERE (a.enabled AND (a.deleted_at IS NULL) AND ((a.owner_user_id IS NULL) OR ((a.kind <> 'gateway'::text) AND u.enabled AND (u.deleted_at IS NULL) AND (a.kind = ANY (COALESCE(string_to_array(( SELECT settings.value
           FROM settings
          WHERE (settings.key = 'user_app_kinds'::text)), ','::text), ARRAY[a.kind]))))));
