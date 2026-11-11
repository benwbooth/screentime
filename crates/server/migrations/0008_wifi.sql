-- The household's Wi-Fi time: one balance for everyone, and the router that enforces it.
-- Off until a parent sets the router password and turns it on, so deploying never cuts the Wi-Fi.
CREATE TABLE wifi (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    enabled INTEGER NOT NULL DEFAULT 0,
    -- Seconds left as of counted_at. While enabled they run down in real time.
    remaining_seconds INTEGER NOT NULL DEFAULT 0,
    counted_at TEXT NOT NULL,
    router_url TEXT NOT NULL DEFAULT 'http://192.168.1.1',
    router_user TEXT NOT NULL DEFAULT 'admin',
    -- Kept in the server's data directory only; never sent back to the browser.
    router_password TEXT
);
INSERT INTO wifi (id, counted_at) VALUES (1, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'));
