-- Call flows for the admin call log: one row per call (Linkedid) as seen by patchbay-web's call
-- tracking (calltrack.rs): where it started (device, trunk), the number it came in on, the targets it
-- passed (person, group, IVR), every Dial leg (device or trunk, caller ID sent, result) and who
-- answered. Calls from before this table (or while patchbay-web was down) fall back to the CDR.
-- Retention: PBX_RETENTION_CALLS_DAYS, like cdr.

CREATE TABLE call_flows (
    linkedid text PRIMARY KEY,
    started_at timestamp with time zone NOT NULL,
    duration_s integer NOT NULL,
    talk_s integer,
    flow jsonb NOT NULL
);

CREATE INDEX call_flows_started ON call_flows (started_at);

GRANT SELECT, INSERT, DELETE ON TABLE call_flows TO pbxweb;
