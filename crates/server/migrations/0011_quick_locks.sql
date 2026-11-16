-- A parent's one-shot "lock now": each computer (host '*' for all of them) locks the kid's
-- sessions once and acknowledges it, and nothing keeps them locked afterwards. Computers only
-- see it for a couple of minutes, so one that was off doesn't lock hours later.
CREATE TABLE quick_locks (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    host TEXT NOT NULL COLLATE NOCASE,
    -- Unix seconds.
    created_at INTEGER NOT NULL
);
CREATE INDEX quick_locks_user ON quick_locks (user_id, created_at);

CREATE TABLE quick_lock_acks (
    quick_lock_id INTEGER NOT NULL REFERENCES quick_locks(id) ON DELETE CASCADE,
    host TEXT NOT NULL COLLATE NOCASE,
    PRIMARY KEY (quick_lock_id, host)
);
