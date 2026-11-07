-- A newer request from the same kid replaces an older unanswered one.
CREATE TABLE time_requests_new (
    id INTEGER PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    host TEXT NOT NULL,
    requested_minutes INTEGER,
    reason TEXT,
    status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'granted', 'denied', 'replaced')),
    granted_minutes INTEGER,
    action_token_hash TEXT NOT NULL,
    created_at TEXT NOT NULL,
    resolved_at TEXT
);
INSERT INTO time_requests_new SELECT id, user_id, host, requested_minutes, reason, status, granted_minutes,
    action_token_hash, created_at, resolved_at FROM time_requests;
DROP TABLE time_requests;
ALTER TABLE time_requests_new RENAME TO time_requests;
-- Only the newest unanswered request per kid stays open.
UPDATE time_requests SET status = 'replaced', resolved_at = created_at
WHERE status = 'pending' AND id NOT IN (SELECT MAX(id) FROM time_requests WHERE status = 'pending' GROUP BY user_id);
