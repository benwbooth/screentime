-- Stretches of continuous use, for the per-day session list.
CREATE TABLE play_sessions (
    id INTEGER PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    host TEXT NOT NULL,
    day TEXT NOT NULL,
    started_at TEXT NOT NULL,
    ended_at TEXT NOT NULL,
    minutes INTEGER NOT NULL
);
CREATE INDEX play_sessions_by_day ON play_sessions (user_id, day);
