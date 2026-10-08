-- Current development schema. No compatibility with earlier experimental stores.
-- Times are signed Unix milliseconds. IDs are lowercase 128-bit hex strings;
-- format validation, ownership, quota and state transitions also require code.


CREATE TABLE account (
    id INTEGER PRIMARY KEY,
    login TEXT NOT NULL UNIQUE,
    status TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'disabled')),
    quota_bytes INTEGER NOT NULL CHECK (quota_bytes >= 0),
    used_bytes INTEGER NOT NULL DEFAULT 0 CHECK (used_bytes >= 0),
    auth_epoch INTEGER NOT NULL DEFAULT 1 CHECK (auth_epoch > 0),
    created_at_ms INTEGER NOT NULL
) STRICT;

CREATE TABLE credential (
    id INTEGER PRIMARY KEY,
    account_id INTEGER NOT NULL REFERENCES account(id),
    selector TEXT NOT NULL UNIQUE,
    label TEXT NOT NULL,
    password_phc TEXT NOT NULL,
    scope TEXT NOT NULL CHECK (scope IN ('mail', 'read_only')),
    revoked_at_ms INTEGER,
    last_used_at_ms INTEGER,
    created_at_ms INTEGER NOT NULL
) STRICT;

-- Canonical local addresses only. Receiving and send-as are separate rights.
CREATE TABLE address (
    address TEXT PRIMARY KEY,
    account_id INTEGER NOT NULL REFERENCES account(id),
    receive_enabled INTEGER NOT NULL DEFAULT 1 CHECK (receive_enabled IN (0, 1)),
    send_enabled INTEGER NOT NULL DEFAULT 0 CHECK (send_enabled IN (0, 1))
) STRICT;

CREATE TABLE mailbox (
    id INTEGER PRIMARY KEY,
    account_id INTEGER NOT NULL REFERENCES account(id),
    name TEXT NOT NULL,
    uidvalidity INTEGER NOT NULL CHECK (uidvalidity BETWEEN 1 AND 4294967295),
    -- 2^32 means exhausted; it must never be assigned as a UID.
    uidnext INTEGER NOT NULL DEFAULT 1 CHECK (uidnext BETWEEN 1 AND 4294967296),
    event_seq INTEGER NOT NULL DEFAULT 0 CHECK (event_seq >= 0),
    UNIQUE (account_id, name)
) STRICT;

-- Subscriptions can outlive mailbox deletion, hence name rather than mailbox FK.
CREATE TABLE subscription (
    account_id INTEGER NOT NULL REFERENCES account(id),
    mailbox_name TEXT NOT NULL,
    PRIMARY KEY (account_id, mailbox_name)
) STRICT;

CREATE TABLE blob (
    id TEXT PRIMARY KEY CHECK (length(id) = 32),
    size_bytes INTEGER NOT NULL CHECK (size_bytes >= 0),
    sha256 TEXT NOT NULL CHECK (length(sha256) = 64),
    created_at_ms INTEGER NOT NULL
) STRICT;

CREATE TABLE message (
    id TEXT PRIMARY KEY CHECK (length(id) = 32),
    ingest_key TEXT NOT NULL UNIQUE,
    blob_id TEXT NOT NULL REFERENCES blob(id),
    source TEXT NOT NULL CHECK (source IN ('smtp', 'submission', 'append', 'dsn', 'import')),
    -- Empty string is the null reverse-path. Header From is not a substitute.
    reverse_path TEXT NOT NULL,
    authenticated_account_id INTEGER REFERENCES account(id),
    header_message_id TEXT,
    accepted_at_ms INTEGER NOT NULL
) STRICT;
CREATE INDEX message_blob ON message(blob_id);

CREATE TABLE delivery (
    id TEXT PRIMARY KEY CHECK (length(id) = 32),
    message_id TEXT NOT NULL REFERENCES message(id),
    recipient TEXT NOT NULL,
    route TEXT NOT NULL CHECK (route IN ('local', 'relay', 'direct')),
    destination_domain TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN
        ('pending', 'leased', 'deferred', 'uncertain', 'delivered', 'failed', 'hold')),
    attempts INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    next_attempt_at_ms INTEGER NOT NULL,
    expires_at_ms INTEGER NOT NULL,
    lease_token TEXT,
    lease_until_ms INTEGER,
    generation INTEGER NOT NULL DEFAULT 0 CHECK (generation >= 0),
    last_smtp_code INTEGER CHECK (last_smtp_code BETWEEN 200 AND 599),
    diagnostic TEXT CHECK (length(diagnostic) <= 2048),
    lifecycle_reason TEXT CHECK (lifecycle_reason IN ('smtp_permanent','expired','content','counter','manual')),
    possibly_delivered INTEGER NOT NULL DEFAULT 0 CHECK (possibly_delivered IN (0,1)),
    closed_at_ms INTEGER,
    notification_state TEXT NOT NULL DEFAULT 'pending' CHECK (notification_state IN ('pending','created','suppressed')),
    notification_error TEXT CHECK (length(notification_error)<=256),
    notification_due_ms INTEGER NOT NULL DEFAULT 0,
    UNIQUE (message_id, recipient),
    CHECK ((lease_token IS NULL) = (lease_until_ms IS NULL)),
    CHECK (state != 'leased' OR lease_token IS NOT NULL)
) STRICT;

CREATE TABLE mailbox_message (
    mailbox_id INTEGER NOT NULL REFERENCES mailbox(id),
    uid INTEGER NOT NULL CHECK (uid BETWEEN 1 AND 4294967295),
    message_id TEXT NOT NULL REFERENCES message(id),
    -- One original local delivery creates at most one mailbox entry.
    -- APPEND and COPY create new entries with a NULL delivery_id.
    delivery_id TEXT UNIQUE REFERENCES delivery(id),
    flags INTEGER NOT NULL DEFAULT 0 CHECK (flags >= 0),
    keywords TEXT NOT NULL DEFAULT '' CHECK (length(keywords) <= 4096),
    recent_unclaimed INTEGER NOT NULL DEFAULT 1 CHECK (recent_unclaimed IN (0, 1)),
    internaldate_ms INTEGER NOT NULL,
    PRIMARY KEY (mailbox_id, uid)
) STRICT;
CREATE INDEX mailbox_message_message ON mailbox_message(message_id);

CREATE TABLE mailbox_event (
    mailbox_id INTEGER NOT NULL REFERENCES mailbox(id),
    event_seq INTEGER NOT NULL CHECK (event_seq > 0),
    kind TEXT NOT NULL CHECK (kind IN ('append', 'flags', 'expunge', 'rename')),
    uid INTEGER CHECK (uid BETWEEN 1 AND 4294967295),
    payload BLOB NOT NULL CHECK (length(payload) <= 65536),
    created_at_ms INTEGER NOT NULL,
    PRIMARY KEY (mailbox_id, event_seq)
) STRICT;

CREATE TABLE notification (
    delivery_id TEXT NOT NULL REFERENCES delivery(id),
    kind TEXT NOT NULL CHECK (kind IN ('failure')),
    report_message_id TEXT NOT NULL REFERENCES message(id),
    created_at_ms INTEGER NOT NULL,
    PRIMARY KEY (delivery_id, kind)
) STRICT;

CREATE TABLE backup_pin (
    backup_id TEXT NOT NULL,
    blob_id TEXT NOT NULL REFERENCES blob(id),
    created_at_ms INTEGER NOT NULL,
    PRIMARY KEY (backup_id, blob_id)
) STRICT;

CREATE TABLE maintenance_run (
    id TEXT PRIMARY KEY CHECK (length(id) = 32),
    started_at_ms INTEGER NOT NULL,
    completed_at_ms INTEGER,
    min_age_seconds INTEGER NOT NULL CHECK (min_age_seconds >= 0),
    status TEXT NOT NULL CHECK (status IN ('running', 'complete', 'interrupted'))
) STRICT;

CREATE TABLE gc_action (
    run_id TEXT NOT NULL REFERENCES maintenance_run(id),
    kind TEXT NOT NULL CHECK (kind IN ('staging', 'orphan')),
    object_id TEXT NOT NULL CHECK (length(object_id) = 32),
    size_bytes INTEGER NOT NULL CHECK (size_bytes >= 0),
    status TEXT NOT NULL CHECK (status IN ('planned', 'deleted')),
    PRIMARY KEY (run_id, kind, object_id)
) STRICT;

CREATE TABLE queue_message (
    message_id TEXT PRIMARY KEY REFERENCES message(id),
    body_mode TEXT NOT NULL CHECK (body_mode IN ('7bit', '8bitmime')),
    max_age_seconds INTEGER NOT NULL CHECK (max_age_seconds BETWEEN 1 AND 604800)
) STRICT;

CREATE TABLE queue_lease (
    delivery_id TEXT PRIMARY KEY REFERENCES delivery(id),
    phase TEXT NOT NULL CHECK (phase IN ('ready', 'body'))
) STRICT;

CREATE INDEX queue_ready ON delivery(next_attempt_at_ms, id)
    WHERE route='relay' AND state IN ('pending', 'deferred');
CREATE INDEX queue_recovery ON delivery(id) WHERE route='relay' AND state='leased';
CREATE INDEX queue_list ON delivery(id) WHERE route='relay';

CREATE INDEX queue_expiry ON delivery(expires_at_ms,id)
    WHERE route='relay' AND state IN ('pending','deferred');
CREATE INDEX queue_notification ON delivery(notification_due_ms,id)
    WHERE route='relay' AND state='failed' AND notification_state='pending';

CREATE TABLE queue_admin_event (
    id INTEGER PRIMARY KEY,
    delivery_id TEXT NOT NULL REFERENCES delivery(id),
    action TEXT NOT NULL CHECK (action IN ('hold','retry','close_unknown')),
    before_state TEXT NOT NULL,
    after_state TEXT NOT NULL,
    note TEXT NOT NULL CHECK (length(note) BETWEEN 1 AND 512),
    allow_duplicate INTEGER NOT NULL CHECK (allow_duplicate IN (0,1)),
    extend_expired INTEGER NOT NULL CHECK (extend_expired IN (0,1)),
    created_at_ms INTEGER NOT NULL
) STRICT;
CREATE INDEX queue_admin_delivery ON queue_admin_event(delivery_id,id);
