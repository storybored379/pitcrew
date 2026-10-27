CREATE TABLE IF NOT EXISTS web_logins (digest TEXT PRIMARY KEY, pin INTEGER NOT NULL, expires INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS web_accounts (account TEXT PRIMARY KEY, token TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS web_sessions (digest TEXT PRIMARY KEY, account TEXT NOT NULL, csrf TEXT NOT NULL, expires INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS web_link_codes (digest TEXT PRIMARY KEY, account TEXT NOT NULL, expires INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS web_links (channel TEXT NOT NULL, user TEXT NOT NULL, account TEXT NOT NULL, PRIMARY KEY(channel,user));
CREATE TABLE IF NOT EXISTS web_link_attempts (actor TEXT PRIMARY KEY, bucket INTEGER NOT NULL, count INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS web_intakes (id INTEGER PRIMARY KEY AUTOINCREMENT, channel TEXT NOT NULL, message TEXT NOT NULL, account TEXT NOT NULL, conversation TEXT NOT NULL, response TEXT NOT NULL, created INTEGER NOT NULL, UNIQUE(channel,message));
CREATE TABLE IF NOT EXISTS web_intake_deliveries (channel TEXT NOT NULL, message TEXT NOT NULL, account TEXT NOT NULL, state TEXT NOT NULL, PRIMARY KEY(channel,message,account));
CREATE TABLE IF NOT EXISTS web_rates (action TEXT NOT NULL, actor TEXT NOT NULL, bucket INTEGER NOT NULL, count INTEGER NOT NULL, PRIMARY KEY(action,actor));

-- W1 writes this immutable, typed handoff; the bridge reads it. Timestamps are
-- Unix seconds. agent_file contains the JSON FileIdentity {id, revision}.
CREATE TABLE IF NOT EXISTS diagnoses (
    report_id INTEGER PRIMARY KEY REFERENCES web_intakes(id),
    decided_at INTEGER NOT NULL,
    decider_kind TEXT NOT NULL,
    decision TEXT NOT NULL,
    evidence_summary TEXT NOT NULL,
    resource TEXT NOT NULL,
    library_id TEXT NOT NULL,
    arr_instance_id TEXT NOT NULL,
    arr_item_id INTEGER NOT NULL,
    arr_file_id INTEGER NOT NULL,
    agent_file TEXT,
    agent_observed_at INTEGER,
    access_observed_at INTEGER NOT NULL,
    access_expires_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS diagnosis_comparisons (
    report_id INTEGER PRIMARY KEY REFERENCES diagnoses(report_id),
    comparison TEXT NOT NULL
);
