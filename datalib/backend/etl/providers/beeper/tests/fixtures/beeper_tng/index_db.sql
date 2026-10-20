-- ST:TNG-themed `index.db`, the Beeper Texts desktop app's cache.
--
-- The four tables the download mirrors carry the real app's DDL, column
-- for column and key for key: `mx_room_messages` is keyed by a local
-- `id`, as the real one is, which is why the mirror keys it on
-- `(roomID, eventID)` instead. The app's other tables (FTS, accounts,
-- …) are left out; nothing reads them.
--
-- Laid out by //tests/fixtures:make_beeper_texts.py.
--
-- The "self" user mxid is @picard:beeper.com. Conversation targets:
-- Mr. Data (Signal), Dr. Crusher (Signal), Cmdr. Riker (Google Chat),
-- and #bridge (Slack). Activity spans 2024-03 and 2024-04 so the
-- per-month period bucketing has something to do.

PRAGMA journal_mode = DELETE;

-- ─────────────────────────────────────────────────────────────────────
-- threads
-- ─────────────────────────────────────────────────────────────────────

CREATE TABLE 'threads' (
    'threadID' VARCHAR NOT NULL PRIMARY KEY,
    'accountID' VARCHAR,
    'thread' JSON NOT NULL,
    'timestamp' INTEGER DEFAULT 0
  , is_label INTEGER DEFAULT 0, room_type TEXT);

-- Signal DM with Mr. Data. local-signal bridge — its megabridge.db
-- holds Signal's own ids for these messages.
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

-- Slack's #bridge, through the cloud Slack bridge: the channel and the
-- workspace are the network's own ids, `<team>-<channel>`.
INSERT INTO threads (threadID, accountID, thread, timestamp) VALUES (
    '!tng-slack-bridge:beeper.com',
    'slackgo.T1701D-U0PICARD',
    json_object(
        'id',          '!tng-slack-bridge:beeper.com',
        'accountID',   'slackgo.T1701D-U0PICARD',
        'type',        'group',
        'title',       '#bridge',
        'extra', json_object(
            'bridge', json_object(
                'com.beeper.bridge_name', 'slack',
                'channel', json_object(
                    'id',              'T1701D-C0BRIDGE',
                    'fi.mau.receiver', 'T1701D-U0PICARD'
                ),
                'network', json_object('id', 'T1701D'),
                'protocol', json_object(
                    'id',          'slack',
                    'displayname', 'Slack'
                )
            )
        )
    ),
    1711040400000
);

-- $space sentinel — every Beeper account has at least one of these
-- (Beeper-internal grouping rooms). Render skips them; here to verify
-- the filter is exercised.
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

CREATE TABLE 'participants' (
  'account_id' TEXT NOT NULL,
  'room_id' TEXT NOT NULL,
  'id' TEXT NOT NULL,
  'full_name' TEXT DEFAULT NULL,
  'nickname' TEXT DEFAULT NULL,
  'img_url' TEXT DEFAULT NULL,
  'is_verified' INTEGER DEFAULT NULL,
  'cannot_message' INTEGER DEFAULT NULL,
  'is_self' INTEGER DEFAULT NULL,
  'is_network_bot' INTEGER DEFAULT NULL,
  'added_by' TEXT DEFAULT NULL,
  'is_admin' INTEGER DEFAULT NULL,
  'is_pending' INTEGER DEFAULT NULL,
  'has_exited' INTEGER DEFAULT NULL,
  PRIMARY KEY('room_id', 'id')
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
        '@googlechat_115552341:beeper.local',              'William T. Riker', 0),
    ('slackgo.T1701D-U0PICARD', '!tng-slack-bridge:beeper.com',
        '@picard:beeper.com',                              'Jean-Luc Picard', 1),
    ('slackgo.T1701D-U0PICARD', '!tng-slack-bridge:beeper.com',
        '@slackgo_T1701D-U0WORF:beeper.com',               'Worf',            0);

-- ─────────────────────────────────────────────────────────────────────
-- mx_room_messages
-- ─────────────────────────────────────────────────────────────────────
--
-- Written into `tng_messages`, a temporary table of just the columns
-- that vary here, then copied into the real table in `hsOrder` order,
-- the order the app gives its rows their `id`.

CREATE TABLE 'mx_room_messages' (
    'id' INTEGER PRIMARY KEY AUTOINCREMENT,
    'roomID' TEXT NOT NULL,
    'eventID' TEXT NOT NULL,
    'senderContactID' TEXT NOT NULL,
    'timestamp' INTEGER NOT NULL,
    'isEdited' INTEGER NOT NULL,
    'lastEditionID' TEXT,
    'isDetached' INTEGER NOT NULL DEFAULT 0,
    'lastEditionTimestamp' INTEGER,
    'lastEditionOrder' INTEGER,
    'isDeleted' INTEGER NOT NULL,
    'isEncrypted' INTEGER NOT NULL,
    'inReplyToID' TEXT,
    'isReplyThreadFallback' INTEGER,
    'type' TEXT NOT NULL,
    'hsOrder' INTEGER NOT NULL,
    'isSentByMe' INTEGER NOT NULL,
    'replyThreadID' TEXT,
    'replyCount' INTEGER,
    'replyThreadRootID' TEXT,
    'protocol' TEXT,
    'shouldNotify' INTEGER,
    'shouldPreview' INTEGER,
    'countsAsUnread' INTEGER,
    'lastDeletionID' TEXT,
    'editedType' TEXT,
    'mentions' TEXT,
    'message' JSON NOT NULL,
    'sendStatus' JSON,
    'text_content' TEXT,
    'text_formattedContent' TEXT,
    'text_format' TEXT
  , echo_echoID VARCHAR, echo_state VARCHAR, echo_canAutoRetry INTEGER DEFAULT 0, echo_isRetryScheduled INTEGER DEFAULT 0, echo_retryCount INTEGER DEFAULT 0, echo_errorCode INTEGER DEFAULT NULL, echo_errorMessage VARCHAR, echo_groupID VARCHAR, echo_groupIndex INTEGER, seenByMeAt INTEGER DEFAULT NULL, message_size INTEGER DEFAULT 0, recipientReadTimerStartedAt INTEGER DEFAULT NULL, threadRootEventID TEXT DEFAULT NULL, countsAsUnreadInThread INTEGER DEFAULT NULL);

CREATE TEMP TABLE tng_messages (
    eventID TEXT, roomID TEXT, senderContactID TEXT, timestamp INTEGER,
    type TEXT, hsOrder INTEGER, isDeleted INTEGER, isEdited INTEGER,
    lastEditionID TEXT, inReplyToID TEXT, text_content TEXT, message TEXT
);

-- ───── Signal: Data (March 2024) ─────
INSERT INTO tng_messages VALUES (
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

INSERT INTO tng_messages VALUES (
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
INSERT INTO tng_messages VALUES (
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
INSERT INTO tng_messages VALUES (
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

INSERT INTO tng_messages VALUES (
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
INSERT INTO tng_messages VALUES (
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

INSERT INTO tng_messages VALUES (
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
INSERT INTO tng_messages VALUES (
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

INSERT INTO tng_messages VALUES (
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
INSERT INTO tng_messages VALUES (
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
INSERT INTO tng_messages VALUES (
    '$tng-riker-hidden-1:beeper.local',
    '!tng-gchat-riker:beeper.local',
    '@googlechatbot:beeper.local',
    1710943100000,
    'HIDDEN', 299, 0, 0, NULL, NULL,
    'm.room.create',
    json_object('extra', json_object('eventType', 'm.room.create'))
);
INSERT INTO tng_messages VALUES (
    '$tng-riker-hidden-2:beeper.local',
    '!tng-gchat-riker:beeper.local',
    '@googlechatbot:beeper.local',
    1710943110000,
    'HIDDEN', 298, 0, 0, NULL, NULL,
    'm.bridge',
    json_object('extra', json_object('eventType', 'm.bridge'))
);

-- ───── Slack: #bridge (March 2024) — cloud bridge ─────
INSERT INTO tng_messages VALUES (
    '$tng-slack-001:beeper.com',
    '!tng-slack-bridge:beeper.com',
    '@slackgo_T1701D-U0WORF:beeper.com',
    1711040400000,
    'TEXT', 400, 0, 0, NULL, NULL,
    NULL,
    json_object(
        'eventID',  '$tng-slack-001:beeper.com',
        'senderID', '@slackgo_T1701D-U0WORF:beeper.com',
        'timestamp', 1711040400000,
        'text', 'Captain, a Romulan warbird is decloaking off the port bow.',
        'attachments', json('[]')
    )
);

INSERT INTO tng_messages VALUES (
    '$tng-slack-002:beeper.com',
    '!tng-slack-bridge:beeper.com',
    '@picard:beeper.com',
    1711040460000,
    'TEXT', 401, 0, 0, NULL, '$tng-slack-001:beeper.com',
    NULL,
    json_object(
        'eventID',  '$tng-slack-002:beeper.com',
        'senderID', '@picard:beeper.com',
        'timestamp', 1711040460000,
        'text', 'Shields up. Red alert.',
        'attachments', json('[]')
    )
);

INSERT INTO mx_room_messages (
    roomID, eventID, senderContactID, timestamp, isEdited, lastEditionID,
    isDeleted, isEncrypted, inReplyToID, type, hsOrder, isSentByMe,
    message, text_content
)
SELECT roomID, eventID, senderContactID, timestamp, isEdited, lastEditionID,
       isDeleted, 0, inReplyToID, type, hsOrder,
       senderContactID = '@picard:beeper.com', message, text_content
FROM tng_messages ORDER BY hsOrder;

-- ─────────────────────────────────────────────────────────────────────
-- mx_reactions
-- ─────────────────────────────────────────────────────────────────────
--
-- Two reactions on Data's image, one of them a month late.

CREATE TABLE 'mx_reactions' (
    'roomID' VARCHAR NOT NULL,
    'reactionID' VARCHAR NOT NULL,
    'eventID' VARCHAR NOT NULL,
    'senderID' VARCHAR NOT NULL,
    'description' VARCHAR NOT NULL,
    'path' VARCHAR,
    'order' INTEGER NOT NULL,
    'timestamp' INTEGER NOT NULL,
    'isEdited' INTEGER NOT NULL,
    'isDeleted' INTEGER NOT NULL,
    'isSentByMe' INTEGER NOT NULL,
    'echoInfo' TEXT,
    'sendStatus' JSON,
    'reaction' JSON,
    PRIMARY KEY (roomID, reactionID)
  );

INSERT INTO mx_reactions (roomID, reactionID, eventID, senderID, description,
    "order", timestamp, isEdited, isDeleted, isSentByMe, reaction) VALUES (
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
INSERT INTO mx_reactions (roomID, reactionID, eventID, senderID, description,
    "order", timestamp, isEdited, isDeleted, isSentByMe, reaction) VALUES (
    '!tng-data:ba_TNG.local-signal.localhost',
    '$tng-data-react-002:ba_TNG.local-signal.localhost',
    '$tng-data-003:ba_TNG.local-signal.localhost',
    '@signal_+15551234001:local-signal.localhost',
    '🤔',
    0, 1712100000000, 0, 0, 0,
    json_object('emoji', json('true'), 'reactionKey', '🤔')
);
