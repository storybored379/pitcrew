CREATE TABLE release_exclusions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    exclusion_key TEXT NOT NULL UNIQUE,
    client TEXT NOT NULL,
    spec TEXT NOT NULL,
    expires_at INTEGER NOT NULL,
    pinned INTEGER NOT NULL,
    released_at INTEGER
);
CREATE INDEX active_release_exclusions ON release_exclusions(client, released_at, expires_at);
CREATE TABLE policy_audit (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    actor TEXT NOT NULL,
    action TEXT NOT NULL,
    subject TEXT NOT NULL,
    note TEXT NOT NULL,
    at INTEGER NOT NULL
);
CREATE TRIGGER policy_audit_no_update BEFORE UPDATE ON policy_audit BEGIN SELECT RAISE(ABORT, 'policy audit is append-only'); END;
CREATE TRIGGER policy_audit_no_delete BEFORE DELETE ON policy_audit BEGIN SELECT RAISE(ABORT, 'policy audit is append-only'); END;
CREATE TABLE client_capabilities (
    client TEXT PRIMARY KEY,
    ledger TEXT NOT NULL
);
CREATE TABLE file_revisions (
    revision INTEGER PRIMARY KEY AUTOINCREMENT,
    client TEXT NOT NULL,
    resource TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    observed_at INTEGER NOT NULL
);
CREATE TABLE current_file_revisions (
    client TEXT NOT NULL,
    resource TEXT NOT NULL,
    revision INTEGER NOT NULL REFERENCES file_revisions(revision),
    PRIMARY KEY(client, resource)
);
CREATE TABLE item_cooldowns (
    resource TEXT PRIMARY KEY,
    until_at INTEGER NOT NULL
);
