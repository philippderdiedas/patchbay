-- Webhooks (webhooks.rs): static hook definitions in /config/webhooks.d/*.toml; every event that
-- matches a hook is queued here and delivered with retries. The payload is the template data at the
-- time of the event; the request is rendered from the hook's templates when it is sent.

CREATE TABLE webhook_deliveries (
    id bigserial PRIMARY KEY,
    hook text NOT NULL,
    event text NOT NULL,
    payload jsonb NOT NULL,
    -- pending | sending | sent | failed
    status text NOT NULL DEFAULT 'pending',
    attempts integer NOT NULL DEFAULT 0,
    next_at timestamp with time zone NOT NULL DEFAULT now(),
    created_at timestamp with time zone NOT NULL DEFAULT now(),
    sent_at timestamp with time zone,
    last_status integer,
    -- error or the start of the answer
    last_error text,
    CONSTRAINT webhook_deliveries_status_check CHECK (status IN ('pending', 'sending', 'sent', 'failed'))
);
CREATE INDEX webhook_deliveries_due ON webhook_deliveries (next_at) WHERE status = 'pending';
CREATE INDEX webhook_deliveries_created ON webhook_deliveries (created_at);

GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE webhook_deliveries TO pbxweb;
GRANT USAGE ON SEQUENCE webhook_deliveries_id_seq TO pbxweb;

-- External phone books: the cards' X- properties (e.g. X-DOLIBARR-EXTRA-EXT-CRM-ID) for templates,
-- as {"dolibarr_extra_ext_crm_id": "296", …}
ALTER TABLE external_contacts ADD COLUMN extra jsonb NOT NULL DEFAULT '{}';
