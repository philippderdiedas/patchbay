-- Chat posts that show a voicemail (delivery to a group channel or a person, "Play" copies):
-- kept as long as the voicemail exists, so heard / unheard / delete reach every copy and a
-- deleted voicemail's posts are deleted in the chat as well (patchbay-notify, sync_voicemail).
CREATE TABLE voicemail_chat_posts (
    voicemail_id integer NOT NULL REFERENCES voicemails(id) ON DELETE CASCADE,
    platform text NOT NULL,
    channel text NOT NULL,
    post_id text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (platform, post_id)
);
CREATE INDEX voicemail_chat_posts_vm ON voicemail_chat_posts (voicemail_id);
GRANT SELECT, INSERT, DELETE ON TABLE voicemail_chat_posts TO pbxweb;
