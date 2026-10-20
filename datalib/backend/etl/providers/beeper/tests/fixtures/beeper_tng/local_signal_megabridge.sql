-- ST:TNG-themed `local-signal/megabridge.db`: the on-device Signal
-- bridge's own record of each message, pairing the Matrix event (`mxid`)
-- with Signal's id for it.
--
-- `message` is the real bridgev2 table, column for column, keyed by a
-- local `rowid` as the real one is; its foreign keys to the bridge's
-- `portal` and `ghost` tables are left out with those tables, which
-- nothing reads.
--
-- Laid out by //tests/fixtures:make_beeper_texts.py.

PRAGMA journal_mode = DELETE;

CREATE TABLE message (
    rowid              INTEGER PRIMARY KEY,

    bridge_id        TEXT    NOT NULL,
    id               TEXT    NOT NULL,
    part_id          TEXT    NOT NULL,
    mxid             TEXT    NOT NULL,

    room_id          TEXT    NOT NULL,
    room_receiver    TEXT    NOT NULL,
    sender_id        TEXT    NOT NULL,
    sender_mxid      TEXT    NOT NULL,
    timestamp        BIGINT  NOT NULL,
    edit_count       INTEGER NOT NULL,
    double_puppeted  BOOLEAN,
    thread_root_id   TEXT,
    reply_to_id      TEXT,
    reply_to_part_id TEXT,
    send_txn_id      TEXT,
    metadata         jsonb   NOT NULL,

    CONSTRAINT message_real_pkey UNIQUE (bridge_id, room_receiver, id, part_id),
    CONSTRAINT message_mxid_unique UNIQUE (bridge_id, mxid),
    CONSTRAINT message_txn_id_unique UNIQUE (bridge_id, room_receiver, send_txn_id)
);

-- Signal's id for a message is `<sender ACI>|<sent ms>`, which is also
-- how a Signal backup names it.
CREATE TEMP TABLE tng_message (id TEXT, room_id TEXT, sender_mxid TEXT, mxid TEXT);
INSERT INTO tng_message VALUES
    ('tng-picard-account-uuid|1710493200000', 'tng-data-conv-uuid-0001',
        '@picard:beeper.com', '$tng-data-001:ba_TNG.local-signal.localhost'),
    ('tng-data-conv-uuid-0001|1710493500000', 'tng-data-conv-uuid-0001',
        '@signal_+15551234001:local-signal.localhost', '$tng-data-002:ba_TNG.local-signal.localhost'),
    ('tng-picard-account-uuid|1710493800000', 'tng-data-conv-uuid-0001',
        '@picard:beeper.com', '$tng-data-003:ba_TNG.local-signal.localhost'),
    ('tng-data-conv-uuid-0001|1712057400000', 'tng-data-conv-uuid-0001',
        '@signal_+15551234001:local-signal.localhost', '$tng-data-004:ba_TNG.local-signal.localhost'),
    ('tng-picard-account-uuid|1712057700000', 'tng-data-conv-uuid-0001',
        '@picard:beeper.com', '$tng-data-005:ba_TNG.local-signal.localhost'),
    ('tng-crusher-conv-uuid-0002|1712775600000', 'tng-crusher-conv-uuid-0002',
        '@signal_+15551234002:local-signal.localhost', '$tng-bev-001:ba_TNG.local-signal.localhost'),
    ('tng-picard-account-uuid|1712775660000', 'tng-crusher-conv-uuid-0002',
        '@picard:beeper.com', '$tng-bev-002:ba_TNG.local-signal.localhost'),
    -- The bridge keeps a message the app's cache has let go.
    ('tng-data-conv-uuid-0001|1709000000000', 'tng-data-conv-uuid-0001',
        '@signal_+15551234001:local-signal.localhost',
        '$tng-data-000:ba_TNG.local-signal.localhost');

INSERT INTO message (bridge_id, id, part_id, mxid, room_id, room_receiver, sender_id,
                     sender_mxid, timestamp, edit_count, double_puppeted, metadata)
SELECT 'local-signal', id, '', mxid, room_id, 'tng-picard-account-uuid',
       substr(id, 1, instr(id, '|') - 1), sender_mxid,
       CAST(substr(id, instr(id, '|') + 1) AS INTEGER), 0, 0, '{}'
FROM tng_message;
