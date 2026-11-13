-- Whether the last check after a single-shot turn saw nothing alive, as far as the
-- ownership selector can tell. NULL: no cleanup is tracked for the row (a chat or
-- standing session, a start that failed before anything was spawned, or a row from
-- before this existed). 'pending': a single-shot launch has not finalized cleanup.
-- 'verified': sweep ran, output ended, last check clean, no failed read, child reaped.
-- 'unverified': anything else at finalization.
ALTER TABLE sessions ADD COLUMN cleanup TEXT;
