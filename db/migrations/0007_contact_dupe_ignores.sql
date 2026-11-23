-- Duplicate finder (contacts): pairs a person marked "not a duplicate" (undone on the same page).
-- Per person: the decision only hides the pair from their own duplicate list.
CREATE TABLE contact_dupe_ignores (
    user_id integer NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    a integer NOT NULL REFERENCES contacts(id) ON DELETE CASCADE,
    b integer NOT NULL REFERENCES contacts(id) ON DELETE CASCADE,
    created_at timestamp with time zone NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, a, b),
    CHECK (a < b)
);

GRANT SELECT, INSERT, DELETE ON TABLE contact_dupe_ignores TO pbxweb;
