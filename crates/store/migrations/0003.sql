-- Outbound representation and the phase of a currently leased attempt.
-- Historical local messages and delivery rows are unchanged.
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
