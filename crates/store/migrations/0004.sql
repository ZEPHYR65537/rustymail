-- Failure notification outbox, sticky uncertainty and explicit operator history.
-- Do not rewrite already accepted messages or already linked notifications.
ALTER TABLE delivery ADD COLUMN lifecycle_reason TEXT
    CHECK (lifecycle_reason IN ('smtp_permanent','expired','content','counter','manual','legacy'));
ALTER TABLE delivery ADD COLUMN possibly_delivered INTEGER NOT NULL DEFAULT 0
    CHECK (possibly_delivered IN (0,1));
ALTER TABLE delivery ADD COLUMN closed_at_ms INTEGER;
ALTER TABLE delivery ADD COLUMN notification_state TEXT NOT NULL DEFAULT 'pending'
    CHECK (notification_state IN ('pending','created','suppressed'));
ALTER TABLE delivery ADD COLUMN notification_error TEXT CHECK (length(notification_error)<=256);
ALTER TABLE delivery ADD COLUMN notification_due_ms INTEGER NOT NULL DEFAULT 0;

UPDATE delivery SET possibly_delivered=1
    WHERE state='uncertain' OR (state='hold' AND
        coalesce(diagnostic,'') NOT IN ('local outbound content or capability failure','expired or attempt counter exhausted'))
    OR (state IN ('pending','deferred','leased','failed','hold') AND
        (attempts>1 OR diagnostic='explicit administrator retry'));
-- Schema 3 could erase an earlier unknown outcome through an explicit retry.
-- It has no attempt history to prove that an old multi-attempt failure was safe.
UPDATE delivery SET state='uncertain',diagnostic='legacy retry; earlier outcome cannot be excluded'
    WHERE state='failed' AND possibly_delivered=1;
UPDATE delivery SET lifecycle_reason=CASE
    WHEN state='failed' AND last_smtp_code BETWEEN 500 AND 599 THEN 'smtp_permanent'
    WHEN state='failed' THEN 'legacy'
    WHEN state='hold' AND diagnostic='local outbound content or capability failure' THEN 'content'
    WHEN state='hold' THEN 'manual' END;
UPDATE delivery SET notification_state='created'
    WHERE EXISTS(SELECT 1 FROM notification n WHERE n.delivery_id=delivery.id AND n.kind='failure');

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
