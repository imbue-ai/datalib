-- ST:TNG-themed fixture for the Beeper extract path.
--
-- Builds a minimal `index.db` analog with just the columns our
-- extractor reads (see src/extract/index_db.rs). Does NOT include
-- the desktop app's FTS/triggers/etc. — we don't read those.
--
-- Pipe into sqlite3 from `build_fixture.sh`:
--     sqlite3 <BeeperTexts>/index.db < index_db.sql
--
-- The "self" user mxid is @picard:beeper.com. Conversation
-- targets: Mr. Data (Signal), Dr. Crusher (Signal), Cmdr. Riker
-- (Google Chat). Activity spans 2024-03 and 2024-04 so the
-- per-month period bucketing has something to do.

PRAGMA journal_mode = DELETE;

-- ─────────────────────────────────────────────────────────────────────
-- threads
-- ─────────────────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS threads (
    threadID  TEXT NOT NULL PRIMARY KEY,
    accountID TEXT,
    thread    TEXT NOT NULL,            -- JSON
    timestamp INTEGER DEFAULT 0
);

-- Signal DM with Mr. Data. local-signal bridge — has a corresponding
-- megabridge.db that the enrich pass will pair with.
INSERT INTO threads (threadID, accountID, thread, timestamp) VALUES (
    '!tng-data:ba_TNG.local-signal.localhost',
    'local-signal_ba_TNG',
    json_object(
        'id',         '!tng-data:ba_TNG.local-signal.localhost',
        'accountID',  'local-signal_ba_TNG',
        'type',       'single',
        'title',      NULL,
        'description','Signal DM with Mr. Data',
        'isReadOnly', json('false'),
        'extra', json_object(
            'bridge', json_object(
                'com.beeper.bridge_name', 'local-signal',
                'com.beeper.room_type',   'dm',
                'channel', json_object(
                    'id',              'tng-data-conv-uuid-0001',
                    'fi.mau.receiver', 'tng-picard-account-uuid'
                ),
                'protocol', json_object(
                    'id',          'signal',
                    'displayname', 'Signal'
                )
            )
        )
    ),
    1712175600000
);

-- Signal DM with Dr. Crusher.
INSERT INTO threads (threadID, accountID, thread, timestamp) VALUES (
    '!tng-crusher:ba_TNG.local-signal.localhost',
    'local-signal_ba_TNG',
    json_object(
        'id',          '!tng-crusher:ba_TNG.local-signal.localhost',
        'accountID',   'local-signal_ba_TNG',
        'type',        'single',
        'description', 'Signal DM with Dr. Crusher',
        'extra', json_object(
            'bridge', json_object(
                'com.beeper.bridge_name', 'local-signal',
                'com.beeper.room_type',   'dm',
                'channel', json_object(
                    'id',              'tng-crusher-conv-uuid-0002',
                    'fi.mau.receiver', 'tng-picard-account-uuid'
                ),
                'protocol', json_object(
                    'id',          'signal',
                    'displayname', 'Signal'
                )
            )
        )
    ),
    1712775600000
);

-- Google Chat with Cmdr. Riker. Cloud bridge — no local megabridge.
INSERT INTO threads (threadID, accountID, thread, timestamp) VALUES (
    '!tng-gchat-riker:beeper.local',
    'googlechat',
    json_object(
        'id',          '!tng-gchat-riker:beeper.local',
        'accountID',   'googlechat',
        'type',        'single',
        'title',       'William T. Riker',
        'description', 'Google Chat with Cmdr. Riker',
        'extra', json_object(
            'bridge', json_object(
                'com.beeper.bridge_name', 'googlechat',
                'channel', json_object(
                    'id',          'dm:tng-riker-space',
                    'displayname', 'William T. Riker'
                ),
                'protocol', json_object(
                    'id',          'googlechat',
                    'displayname', 'Google Chat'
                )
            )
        )
    ),
    1710943200000
);

-- $space sentinel — every Beeper account has at least one of these
-- (Beeper-internal grouping rooms). Our extractor skips them; here
-- to verify the filter is exercised.
INSERT INTO threads (threadID, accountID, thread, timestamp) VALUES (
    '!tng-internal-space:beeper.local',
    '$space',
    json_object(
        'id',          '!tng-internal-space:beeper.local',
        'accountID',   '$space',
        'type',        'group',
        'title',       'Beeper internal space',
        'extra',       json_object('roomType', 'm.space')
    ),
    1710000000000
);

-- ─────────────────────────────────────────────────────────────────────
-- participants
-- ─────────────────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS participants (
    account_id TEXT NOT NULL,
    room_id    TEXT NOT NULL,
    id         TEXT NOT NULL,
    full_name  TEXT,
    nickname   TEXT,
    img_url    TEXT,
    is_verified INTEGER,
    cannot_message INTEGER,
    is_self    INTEGER,
    is_network_bot INTEGER,
    is_admin   INTEGER,
    PRIMARY KEY(room_id, id)
);

INSERT INTO participants (account_id, room_id, id, full_name, is_self) VALUES
    ('local-signal_ba_TNG', '!tng-data:ba_TNG.local-signal.localhost',
        '@picard:beeper.com',                              'Jean-Luc Picard', 1),
    ('local-signal_ba_TNG', '!tng-data:ba_TNG.local-signal.localhost',
        '@signal_+15551234001:local-signal.localhost',     'Mr. Data',        0),
    ('local-signal_ba_TNG', '!tng-crusher:ba_TNG.local-signal.localhost',
        '@picard:beeper.com',                              'Jean-Luc Picard', 1),
    ('local-signal_ba_TNG', '!tng-crusher:ba_TNG.local-signal.localhost',
        '@signal_+15551234002:local-signal.localhost',     'Dr. Beverly Crusher', 0),
    ('googlechat',          '!tng-gchat-riker:beeper.local',
        '@picard:beeper.com',                              'Jean-Luc Picard', 1),
    ('googlechat',          '!tng-gchat-riker:beeper.local',
        '@googlechat_115552341:beeper.local',              'William T. Riker', 0);

-- ─────────────────────────────────────────────────────────────────────
-- mx_room_messages
-- ─────────────────────────────────────────────────────────────────────
--
-- Minimal subset of the real schema — only the columns the
-- extractor SELECTs. Real Beeper has dozens more (FTS support,
-- echo state, derived content). Fixture stays terse.

CREATE TABLE IF NOT EXISTS mx_room_messages (
    eventID         TEXT NOT NULL,
    roomID          TEXT NOT NULL,
    senderContactID TEXT,
    timestamp       INTEGER NOT NULL,
    type            TEXT NOT NULL,
    hsOrder         INTEGER NOT NULL,
    isDeleted       INTEGER NOT NULL DEFAULT 0,
    isEdited        INTEGER NOT NULL DEFAULT 0,
    lastEditionID   TEXT,
    inReplyToID     TEXT,
    text_content    TEXT,
    message         TEXT NOT NULL,
    PRIMARY KEY(roomID, eventID)
);

-- ───── Signal: Data (March 2024) ─────
INSERT INTO mx_room_messages VALUES (
    '$tng-data-001:ba_TNG.local-signal.localhost',
    '!tng-data:ba_TNG.local-signal.localhost',
    '@picard:beeper.com',
    1710493200000,
    'TEXT', 100, 0, 0, NULL, NULL,
    'Mr. Data, regarding the Iconian artifact analysis...',
    json_object(
        'eventID', '$tng-data-001:ba_TNG.local-signal.localhost',
        'senderID','@picard:beeper.com',
        'timestamp', 1710493200000,
        'text', 'Mr. Data, regarding the Iconian artifact analysis...',
        'attachments', json('[]')
    )
);

INSERT INTO mx_room_messages VALUES (
    '$tng-data-002:ba_TNG.local-signal.localhost',
    '!tng-data:ba_TNG.local-signal.localhost',
    '@signal_+15551234001:local-signal.localhost',
    1710493500000,
    'TEXT', 101, 0, 0, NULL,
    '$tng-data-001:ba_TNG.local-signal.localhost',
    'I have analyzed the artifact, Captain. It appears to be a quantum lattice.',
    json_object(
        'eventID', '$tng-data-002:ba_TNG.local-signal.localhost',
        'senderID','@signal_+15551234001:local-signal.localhost',
        'timestamp', 1710493500000,
        'text', 'I have analyzed the artifact, Captain. It appears to be a quantum lattice.',
        'attachments', json('[]')
    )
);

-- Image: Picard sends a schematic — `localmxc://` (local Signal bridge).
INSERT INTO mx_room_messages VALUES (
    '$tng-data-003:ba_TNG.local-signal.localhost',
    '!tng-data:ba_TNG.local-signal.localhost',
    '@picard:beeper.com',
    1710493800000,
    'IMAGE', 102, 0, 0, NULL, NULL,
    NULL,
    json_object(
        'eventID', '$tng-data-003:ba_TNG.local-signal.localhost',
        'senderID','@picard:beeper.com',
        'timestamp', 1710493800000,
        'attachments', json_array(
            json_object(
                'id',       'localmxc://local-signal/TNGART01',
                'srcURL',   'localmxc://local-signal/TNGART01',
                'fileName', 'iconian-schematic.png',
                'mimeType', 'image/png',
                'fileSize', 1024
            )
        )
    )
);

-- ───── Signal: Data (April 2024) — exercises multi-period ─────
INSERT INTO mx_room_messages VALUES (
    '$tng-data-004:ba_TNG.local-signal.localhost',
    '!tng-data:ba_TNG.local-signal.localhost',
    '@signal_+15551234001:local-signal.localhost',
    1712057400000,
    'TEXT', 103, 0, 0, NULL, NULL,
    'Captain, I have located three similar structures in the Federation database.',
    json_object(
        'eventID', '$tng-data-004:ba_TNG.local-signal.localhost',
        'senderID','@signal_+15551234001:local-signal.localhost',
        'timestamp', 1712057400000,
        'text', 'Captain, I have located three similar structures in the Federation database.',
        'attachments', json('[]')
    )
);

INSERT INTO mx_room_messages VALUES (
    '$tng-data-005:ba_TNG.local-signal.localhost',
    '!tng-data:ba_TNG.local-signal.localhost',
    '@picard:beeper.com',
    1712057700000,
    'TEXT', 104, 0, 0, NULL, NULL,
    'Excellent. Brief the senior staff at oh-eight-hundred.',
    json_object(
        'eventID', '$tng-data-005:ba_TNG.local-signal.localhost',
        'senderID','@picard:beeper.com',
        'timestamp', 1712057700000,
        'text', 'Excellent. Brief the senior staff at oh-eight-hundred.',
        'attachments', json('[]')
    )
);

-- ───── Signal: Crusher (April 2024) — has an edit ─────
INSERT INTO mx_room_messages VALUES (
    '$tng-bev-001:ba_TNG.local-signal.localhost',
    '!tng-crusher:ba_TNG.local-signal.localhost',
    '@signal_+15551234002:local-signal.localhost',
    1712775600000,
    'TEXT', 200, 0, 0, NULL, NULL,
    'Jean-Luc, dinner tonight in my quarters?',
    json_object(
        'eventID',  '$tng-bev-001:ba_TNG.local-signal.localhost',
        'senderID', '@signal_+15551234002:local-signal.localhost',
        'timestamp', 1712775600000,
        'text',     'Jean-Luc, dinner tonight in my quarters?',
        'attachments', json('[]')
    )
);

INSERT INTO mx_room_messages VALUES (
    '$tng-bev-002:ba_TNG.local-signal.localhost',
    '!tng-crusher:ba_TNG.local-signal.localhost',
    '@picard:beeper.com',
    1712775660000,
    'TEXT', 201, 0, 1,
    '$tng-bev-002-edit-1:ba_TNG.local-signal.localhost',
    NULL,
    'Earl Grey, hot — at 19:00.',
    json_object(
        'eventID',   '$tng-bev-002:ba_TNG.local-signal.localhost',
        'senderID',  '@picard:beeper.com',
        'timestamp', 1712775660000,
        'text',      'Earl Grey, hot — at 19:00.',
        'extra',     json_object('lastEditionID', '$tng-bev-002-edit-1:ba_TNG.local-signal.localhost'),
        'attachments', json('[]')
    )
);

-- ───── Google Chat: Riker (March 2024) — cloud bridge ─────
INSERT INTO mx_room_messages VALUES (
    '$tng-riker-001:beeper.local',
    '!tng-gchat-riker:beeper.local',
    '@googlechat_115552341:beeper.local',
    1710943200000,
    'TEXT', 300, 0, 0, NULL, NULL,
    'Captain, the away team report is ready for your review.',
    json_object(
        'eventID',  '$tng-riker-001:beeper.local',
        'senderID', '@googlechat_115552341:beeper.local',
        'timestamp', 1710943200000,
        'text', 'Captain, the away team report is ready for your review.',
        'attachments', json('[]')
    )
);

INSERT INTO mx_room_messages VALUES (
    '$tng-riker-002:beeper.local',
    '!tng-gchat-riker:beeper.local',
    '@picard:beeper.com',
    1710943500000,
    'TEXT', 301, 0, 0, NULL, NULL,
    'Send it through, Number One.',
    json_object(
        'eventID',  '$tng-riker-002:beeper.local',
        'senderID', '@picard:beeper.com',
        'timestamp', 1710943500000,
        'text', 'Send it through, Number One.',
        'attachments', json('[]')
    )
);

-- File attachment: cloud bridge, `mxc://local.beeper.com/...`.
INSERT INTO mx_room_messages VALUES (
    '$tng-riker-003:beeper.local',
    '!tng-gchat-riker:beeper.local',
    '@googlechat_115552341:beeper.local',
    1710943800000,
    'FILE', 302, 0, 0, NULL, NULL,
    NULL,
    json_object(
        'eventID',  '$tng-riker-003:beeper.local',
        'senderID', '@googlechat_115552341:beeper.local',
        'timestamp', 1710943800000,
        'attachments', json_array(
            json_object(
                'id',       'mxc://local.beeper.com/TNGRPT01',
                'srcURL',   'mxc://local.beeper.com/TNGRPT01',
                'fileName', 'away-team-report.pdf',
                'mimeType', 'application/pdf',
                'fileSize', 2048
            )
        )
    )
);

-- HIDDEN events — Beeper marks all sorts of system events HIDDEN.
-- A handful in the Riker room so the translator exercises the
-- HIDDEN render path.
INSERT INTO mx_room_messages VALUES (
    '$tng-riker-hidden-1:beeper.local',
    '!tng-gchat-riker:beeper.local',
    '@googlechatbot:beeper.local',
    1710943100000,
    'HIDDEN', 299, 0, 0, NULL, NULL,
    'm.room.create',
    json_object('extra', json_object('eventType', 'm.room.create'))
);
INSERT INTO mx_room_messages VALUES (
    '$tng-riker-hidden-2:beeper.local',
    '!tng-gchat-riker:beeper.local',
    '@googlechatbot:beeper.local',
    1710943110000,
    'HIDDEN', 298, 0, 0, NULL, NULL,
    'm.bridge',
    json_object('extra', json_object('eventType', 'm.bridge'))
);

-- ─────────────────────────────────────────────────────────────────────
-- mx_reactions
-- ─────────────────────────────────────────────────────────────────────
--
-- A single ❤️ reaction by Picard on Data's image. The matching
-- bridge-side row lives in local_signal_megabridge.sql so the
-- enrich pass has work to do.

CREATE TABLE IF NOT EXISTS mx_reactions (
    roomID       TEXT NOT NULL,
    reactionID   TEXT NOT NULL,
    eventID      TEXT NOT NULL,
    senderID     TEXT NOT NULL,
    description  TEXT NOT NULL,
    "order"      INTEGER NOT NULL DEFAULT 0,
    timestamp    INTEGER NOT NULL,
    isEdited     INTEGER NOT NULL DEFAULT 0,
    isDeleted    INTEGER NOT NULL DEFAULT 0,
    isSentByMe   INTEGER NOT NULL DEFAULT 0,
    reaction     TEXT,
    PRIMARY KEY (roomID, reactionID)
);

INSERT INTO mx_reactions VALUES (
    '!tng-data:ba_TNG.local-signal.localhost',
    '$tng-data-react-001:ba_TNG.local-signal.localhost',
    '$tng-data-003:ba_TNG.local-signal.localhost',
    '@signal_+15551234001:local-signal.localhost',
    '❤️',
    0, 1710494100000, 0, 0, 0,
    json_object('emoji', json('true'), 'reactionKey', '❤️')
);

-- Cross-month reaction: Data reacts in April to the March image.
-- Translator should attach this to the March bucket, not April's.
INSERT INTO mx_reactions VALUES (
    '!tng-data:ba_TNG.local-signal.localhost',
    '$tng-data-react-002:ba_TNG.local-signal.localhost',
    '$tng-data-003:ba_TNG.local-signal.localhost',
    '@signal_+15551234001:local-signal.localhost',
    '🤔',
    0, 1712100000000, 0, 0, 0,
    json_object('emoji', json('true'), 'reactionKey', '🤔')
);
