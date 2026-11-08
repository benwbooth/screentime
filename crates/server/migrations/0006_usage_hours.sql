-- Minutes used in each hour of the day, for the history charts.
CREATE TABLE usage_hours (
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    host TEXT NOT NULL,
    day TEXT NOT NULL,
    hour INTEGER NOT NULL CHECK (hour BETWEEN 0 AND 23),
    minutes INTEGER NOT NULL,
    PRIMARY KEY (user_id, host, day, hour)
);
