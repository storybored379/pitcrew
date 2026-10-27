-- One immutable payload for each reducer-created notification intent.
CREATE TABLE notification_payloads (
    job INTEGER NOT NULL,
    sequence INTEGER NOT NULL,
    payload TEXT NOT NULL,
    PRIMARY KEY (job, sequence),
    FOREIGN KEY (job, sequence) REFERENCES outbox(job, sequence)
);
CREATE TRIGGER notification_payloads_no_update BEFORE UPDATE ON notification_payloads BEGIN SELECT RAISE(ABORT, 'notification payload is immutable'); END;
CREATE TRIGGER notification_payloads_no_delete BEFORE DELETE ON notification_payloads BEGIN SELECT RAISE(ABORT, 'notification payload is immutable'); END;
CREATE TABLE notification_resends (
    request_key TEXT PRIMARY KEY,
    job INTEGER NOT NULL,
    sequence INTEGER NOT NULL,
    actor TEXT NOT NULL,
    note TEXT NOT NULL,
    at INTEGER NOT NULL,
    FOREIGN KEY (job, sequence) REFERENCES outbox(job, sequence)
);
CREATE TRIGGER notification_resends_no_update BEFORE UPDATE ON notification_resends BEGIN SELECT RAISE(ABORT, 'resend audit is append-only'); END;
CREATE TRIGGER notification_resends_no_delete BEFORE DELETE ON notification_resends BEGIN SELECT RAISE(ABORT, 'resend audit is append-only'); END;
