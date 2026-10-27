CREATE TABLE IF NOT EXISTS report_progress (
    report_id INTEGER PRIMARY KEY REFERENCES web_intakes(id),
    job_id INTEGER,
    state TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    viewer_copy_key TEXT NOT NULL,
    job_revision INTEGER NOT NULL DEFAULT 0
);
-- Trusted local collector/access provenance is intentionally separate from the
-- redacted diagnosis and from viewer-controlled intake JSON.
CREATE TABLE IF NOT EXISTS bridge_authority (
    report_id INTEGER PRIMARY KEY REFERENCES web_intakes(id),
    authority TEXT NOT NULL
);
-- Freeze the cross-database command before creating a job. Retrying this exact
-- command recovers a crash between engine commit and progress projection.
CREATE TABLE IF NOT EXISTS bridge_requests (
    key TEXT PRIMARY KEY,
    spec TEXT NOT NULL,
    limits TEXT NOT NULL,
    cooldown INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS bridge_reports (
    report_id INTEGER PRIMARY KEY REFERENCES web_intakes(id),
    request_key TEXT NOT NULL REFERENCES bridge_requests(key)
);
-- A denial or closure suppresses future automatic creation; neither cancels an
-- already frozen repair nor makes any statement about the condition of a file.
CREATE TABLE IF NOT EXISTS bridge_report_controls (
    report_id INTEGER PRIMARY KEY REFERENCES web_intakes(id),
    request_key TEXT NOT NULL UNIQUE,
    action TEXT NOT NULL,
    actor TEXT NOT NULL,
    note TEXT NOT NULL,
    at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS bridge_lifecycle (
    key TEXT PRIMARY KEY,
    report_id INTEGER NOT NULL REFERENCES web_intakes(id),
    job_id INTEGER,
    purpose TEXT NOT NULL,
    payload TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'pending',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TRIGGER IF NOT EXISTS bridge_lifecycle_immutable
BEFORE UPDATE OF key, report_id, job_id, purpose, payload, created_at ON bridge_lifecycle
BEGIN SELECT RAISE(ABORT, 'notification payload is immutable'); END;
CREATE TRIGGER IF NOT EXISTS bridge_lifecycle_no_delete
BEFORE DELETE ON bridge_lifecycle
BEGIN SELECT RAISE(ABORT, 'notification journal is append-only'); END;
CREATE TABLE IF NOT EXISTS bridge_lifecycle_cursor (
    id INTEGER PRIMARY KEY CHECK(id=1), report_id INTEGER NOT NULL
);
INSERT OR IGNORE INTO bridge_lifecycle_cursor VALUES (1,0);
CREATE TABLE IF NOT EXISTS bridge_dispatch_cursor (
    id INTEGER PRIMARY KEY CHECK(id=1), last_key TEXT NOT NULL
);
INSERT OR IGNORE INTO bridge_dispatch_cursor VALUES (1,'');
