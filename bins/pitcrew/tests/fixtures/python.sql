-- Synthetic data only. Column shapes match the legacy SQLite schema plus the
-- item_fixes.kind migration. Unused users/tokens tables are intentionally absent.
CREATE TABLE reports (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  item_key TEXT NOT NULL,
  media_type TEXT NOT NULL,
  title TEXT NOT NULL,
  year INTEGER,
  season INTEGER,
  episode INTEGER,
  issue_type TEXT NOT NULL,
  answers TEXT NOT NULL DEFAULT '[]',
  free_text TEXT,
  action TEXT NOT NULL,
  status TEXT NOT NULL,
  arr_instance TEXT,
  arr_media_id INTEGER,
  old_file_id INTEGER,
  reporter_count INTEGER NOT NULL DEFAULT 1,
  created_at REAL NOT NULL,
  updated_at REAL NOT NULL,
  resolved_at REAL
);
CREATE TABLE audit (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  ts TEXT NOT NULL,
  account_id TEXT,
  username TEXT,
  session_id TEXT,
  ip TEXT,
  action TEXT NOT NULL,
  target TEXT,
  outcome TEXT,
  detail TEXT
);
CREATE TABLE rate_events (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  bucket TEXT NOT NULL,
  key TEXT NOT NULL,
  ts REAL NOT NULL
);
CREATE TABLE item_fixes (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  cooldown_key TEXT NOT NULL,
  item_key TEXT NOT NULL,
  ts REAL NOT NULL,
  kind TEXT NOT NULL DEFAULT 'fix'
);
CREATE TABLE fix_attempts (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  item_key TEXT NOT NULL,
  report_id INTEGER,
  instance TEXT NOT NULL,
  media_kind TEXT NOT NULL,
  media_id INTEGER NOT NULL,
  old_file_id INTEGER,
  old_group TEXT,
  grp TEXT,
  title TEXT,
  target TEXT,
  verdict TEXT NOT NULL,
  ts REAL NOT NULL
);
INSERT INTO reports (id,item_key,media_type,title,issue_type,free_text,action,status,arr_instance,arr_media_id,old_file_id,created_at,updated_at)
VALUES (1,'movie:tmdb:90001','movie','PRIVATE_TITLE_SENTINEL','wont_play','PRIVATE_VIEWER_TEXT_SENTINEL','autofix','resolved','PRIVATE_INSTANCE_SENTINEL',101,201,1000,1100);
INSERT INTO fix_attempts (id,item_key,report_id,instance,media_kind,media_id,old_file_id,title,verdict,ts)
VALUES (1,'movie:tmdb:90001',1,'PRIVATE_INSTANCE_SENTINEL','movie',101,201,'PRIVATE_RELEASE_SENTINEL','ok',1001),
       (2,'movie:tmdb:90001',NULL,'PRIVATE_INSTANCE_SENTINEL','movie',101,201,'PRIVATE_RELEASE_SENTINEL','grabbed',1001);
INSERT INTO audit(ts,account_id,username,session_id,ip,action,target,outcome,detail)
VALUES ('2026-01-01T00:00:00Z','PRIVATE_ACCOUNT_SENTINEL','PRIVATE_VIEWER_SENTINEL','PRIVATE_SESSION_SENTINEL','PRIVATE_IP_SENTINEL','replacement_manual_import','PRIVATE_INSTANCE_SENTINEL: movie:tmdb:90001','ok','report 1: PRIVATE_RELEASE_SENTINEL blocked as not-an-upgrade -> ManualImport [PRIVATE_PATH_SENTINEL]'),
       ('2026-01-01T00:01:00Z',NULL,NULL,NULL,NULL,'replacement_decode_check','movie:tmdb:90001','ok','report 1 file 202: PRIVATE_PATH_SENTINEL'),
       ('2026-01-01T00:02:00Z',NULL,'PRIVATE_VIEWER_SENTINEL',NULL,NULL,'dm_sent','movie:tmdb:90001','ok','report 1 resolved: PRIVATE_MESSAGE_SENTINEL');
INSERT INTO item_fixes(cooldown_key,item_key,ts,kind)
VALUES ('PRIVATE_INSTANCE_SENTINEL:movie:101','movie:tmdb:90001',1001,'fix'),
       ('PRIVATE_INSTANCE_SENTINEL:movie:101','movie:tmdb:90001',1002,'decode_retry');
INSERT INTO rate_events(bucket,key,ts)
VALUES ('research','global',999),('research','PRIVATE_ACCOUNT_SENTINEL',999);
