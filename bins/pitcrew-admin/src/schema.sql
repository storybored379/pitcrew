CREATE TABLE IF NOT EXISTS admin_logins (digest TEXT PRIMARY KEY, pin INTEGER NOT NULL, expires INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS admin_sessions (digest TEXT PRIMARY KEY, account TEXT NOT NULL, csrf TEXT NOT NULL, expires INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS admin_rates (bucket INTEGER PRIMARY KEY, count INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS admin_requests (
 request_key TEXT PRIMARY KEY, actor TEXT NOT NULL, command TEXT NOT NULL,
 note TEXT NOT NULL, state TEXT NOT NULL, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS admin_audit (
 id INTEGER PRIMARY KEY AUTOINCREMENT, request_key TEXT NOT NULL REFERENCES admin_requests(request_key),
 actor TEXT NOT NULL, event TEXT NOT NULL, at INTEGER NOT NULL
);
CREATE TRIGGER IF NOT EXISTS admin_audit_no_update BEFORE UPDATE ON admin_audit BEGIN SELECT RAISE(ABORT, 'admin audit is append-only'); END;
CREATE TRIGGER IF NOT EXISTS admin_audit_no_delete BEFORE DELETE ON admin_audit BEGIN SELECT RAISE(ABORT, 'admin audit is append-only'); END;
