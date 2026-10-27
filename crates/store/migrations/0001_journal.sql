-- The journal is authoritative; snapshots are intentionally not restorable.
CREATE TABLE jobs (
    id INTEGER PRIMARY KEY,
    create_key TEXT NOT NULL UNIQUE,
    spec TEXT NOT NULL,
    revision INTEGER NOT NULL DEFAULT 0,
    lease_owner TEXT,
    lease_fence INTEGER NOT NULL DEFAULT 0,
    lease_until INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE ownership (
    resource TEXT PRIMARY KEY,
    job INTEGER NOT NULL UNIQUE REFERENCES jobs(id)
);
CREATE TABLE budget_reservations (
    job INTEGER PRIMARY KEY REFERENCES jobs(id),
    viewer TEXT NOT NULL,
    at INTEGER NOT NULL,
    units INTEGER NOT NULL
);
CREATE TABLE events (
    job INTEGER NOT NULL REFERENCES jobs(id),
    revision INTEGER NOT NULL,
    event_key TEXT NOT NULL,
    at INTEGER NOT NULL,
    event TEXT NOT NULL,
    PRIMARY KEY (job, revision),
    UNIQUE (job, event_key)
);
CREATE TRIGGER events_no_update BEFORE UPDATE ON events BEGIN SELECT RAISE(ABORT, 'journal is append-only'); END;
CREATE TRIGGER events_no_delete BEFORE DELETE ON events BEGIN SELECT RAISE(ABORT, 'journal is append-only'); END;
CREATE TABLE outbox (
    job INTEGER NOT NULL REFERENCES jobs(id),
    sequence INTEGER NOT NULL,
    intent TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    fence INTEGER,
    PRIMARY KEY (job, sequence)
);
CREATE TABLE observations (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    source TEXT NOT NULL,
    observation_key TEXT NOT NULL,
    payload TEXT NOT NULL,
    at INTEGER NOT NULL,
    UNIQUE (source, observation_key)
);
CREATE TRIGGER observations_no_update BEFORE UPDATE ON observations BEGIN SELECT RAISE(ABORT, 'observations are append-only'); END;
CREATE TRIGGER observations_no_delete BEFORE DELETE ON observations BEGIN SELECT RAISE(ABORT, 'observations are append-only'); END;
