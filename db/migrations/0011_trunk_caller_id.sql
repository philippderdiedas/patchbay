-- Per trunk: take over the caller name / number the peer sends (both on by default, as before).
-- Off: the name is dropped (own contact names still apply); without the number the call shows as the
-- trunk (its name), without a number to call back. Read by the dialplan via TRUNK_INFO.
ALTER TABLE trunks
    ADD COLUMN accept_caller_name boolean NOT NULL DEFAULT true,
    ADD COLUMN accept_caller_number boolean NOT NULL DEFAULT true;
GRANT SELECT (accept_caller_name, accept_caller_number) ON TABLE trunks TO asterisk;
