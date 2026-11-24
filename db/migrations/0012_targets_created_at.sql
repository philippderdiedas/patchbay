-- When a call target row was written: the daily cleanup of targets nothing points to any more
-- (housekeeping.rs) skips fresh rows, which the admin UI writes just before linking them.
ALTER TABLE targets ADD COLUMN created_at timestamptz NOT NULL DEFAULT now();

-- An imported holiday the admin moved to another date stays hidden on its old date (so imports do
-- not bring it back) and is marked, so "Restore" (deleted imported holidays) leaves it alone.
ALTER TABLE holidays ADD COLUMN moved boolean NOT NULL DEFAULT false;
