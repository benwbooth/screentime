-- Wi-Fi becomes a built-in user, so it gets the kids' daily limits, downtime, added time, locks
-- and history. It starts with no limit, so nothing changes until a parent sets a schedule.
INSERT OR IGNORE INTO users (username, display_name) VALUES ('wifi', 'Wi-Fi');

-- Carry over the old balance as time added today.
INSERT INTO bonus_grants (user_id, day, minutes, source, granted_at)
SELECT u.id, date('now', 'localtime'), (w.remaining_seconds + 59) / 60, 'wifi balance',
       strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
FROM wifi w, users u
WHERE w.id = 1 AND u.username = 'wifi' AND w.remaining_seconds > 0;
UPDATE wifi SET remaining_seconds = 0;
