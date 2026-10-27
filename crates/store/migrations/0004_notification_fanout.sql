-- Recipients are indexes into the parent immutable payload. The persisted
-- uncertain state precedes each remote call; confirmations are never retried.
CREATE TABLE notification_recipients (
    job INTEGER NOT NULL,
    sequence INTEGER NOT NULL,
    recipient INTEGER NOT NULL CHECK (recipient >= 0),
    state TEXT NOT NULL CHECK (state IN ('pending','uncertain','confirmed')),
    attempt INTEGER NOT NULL CHECK (attempt >= 0),
    PRIMARY KEY (job, sequence, recipient),
    FOREIGN KEY (job, sequence) REFERENCES notification_payloads(job, sequence)
);
