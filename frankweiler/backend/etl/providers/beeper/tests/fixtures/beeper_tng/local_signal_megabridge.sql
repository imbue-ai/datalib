-- ST:TNG-themed fixture for the megabridge enrichment pass.
--
-- Each message and reaction here pairs `mxid` (the Matrix event
-- id our extractor stored as `native_event_id`) with the bridge's
-- own native id. The enrichment UPDATE walks this table after the
-- index.db pass and stamps `external_event_id` for matched rows.
--
-- Only the columns our enricher SELECTs are present. The real
-- megabridge has dozens of NOT NULL columns + foreign-key
-- constraints; the fixture stays terse.
--
-- Pipe into sqlite3 from `build_fixture.sh`:
--     sqlite3 <BeeperTexts>/local-signal/megabridge.db < local_signal_megabridge.sql

PRAGMA journal_mode = DELETE;

-- ─────────────────────────────────────────────────────────────────────
-- message — mxid ↔ id pairing for Signal messages
-- ─────────────────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS message (
    bridge_id TEXT NOT NULL,
    id        TEXT NOT NULL,
    part_id   TEXT NOT NULL,
    mxid      TEXT NOT NULL,
    PRIMARY KEY(bridge_id, mxid)
);

-- Signal-native ids look like `<sender-account-uuid>|<unix-ms>` on
-- mautrix-signal. Match that shape so the enrichment output looks
-- realistic.
INSERT INTO message VALUES
    ('local-signal', 'tng-picard-account-uuid|1710493200000', '',
        '$tng-data-001:ba_TNG.local-signal.localhost'),
    ('local-signal', 'tng-data-conv-uuid-0001|1710493500000', '',
        '$tng-data-002:ba_TNG.local-signal.localhost'),
    ('local-signal', 'tng-picard-account-uuid|1710493800000', '',
        '$tng-data-003:ba_TNG.local-signal.localhost'),
    ('local-signal', 'tng-data-conv-uuid-0001|1712057400000', '',
        '$tng-data-004:ba_TNG.local-signal.localhost'),
    ('local-signal', 'tng-picard-account-uuid|1712057700000', '',
        '$tng-data-005:ba_TNG.local-signal.localhost'),
    ('local-signal', 'tng-crusher-conv-uuid-0002|1712775600000', '',
        '$tng-bev-001:ba_TNG.local-signal.localhost'),
    ('local-signal', 'tng-picard-account-uuid|1712775660000', '',
        '$tng-bev-002:ba_TNG.local-signal.localhost');

-- A megabridge row whose mxid doesn't match anything in index.db.
-- Enrichment should count this as `events_orphaned`.
INSERT INTO message VALUES
    ('local-signal', 'tng-orphan|9999999999999', '',
        '$tng-orphan-not-in-index-db:ba_TNG.local-signal.localhost');

-- ─────────────────────────────────────────────────────────────────────
-- reaction — pairs the reaction mxid with the bridge-native target
-- ─────────────────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS reaction (
    bridge_id        TEXT NOT NULL,
    message_id       TEXT NOT NULL,
    message_part_id  TEXT NOT NULL,
    sender_id        TEXT NOT NULL,
    sender_mxid      TEXT NOT NULL DEFAULT '',
    emoji_id         TEXT NOT NULL,
    room_id          TEXT NOT NULL,
    room_receiver    TEXT NOT NULL,
    mxid             TEXT NOT NULL,
    timestamp        INTEGER NOT NULL,
    emoji            TEXT NOT NULL,
    PRIMARY KEY (bridge_id, room_receiver, message_id, message_part_id, sender_id, emoji_id)
);

INSERT INTO reaction VALUES
    -- Picard's ❤️ on Data's image (March).
    ('local-signal', 'tng-picard-account-uuid|1710493800000', '',
        'tng-data-conv-uuid-0001', '@signal_+15551234001:local-signal.localhost',
        '❤️', 'tng-data-conv-uuid-0001', 'tng-picard-account-uuid',
        '$tng-data-react-001:ba_TNG.local-signal.localhost',
        1710494100000, '❤️'),
    -- Data's 🤔 (April), reacting to the March image (cross-month).
    ('local-signal', 'tng-picard-account-uuid|1710493800000', '',
        'tng-data-conv-uuid-0001', '@signal_+15551234001:local-signal.localhost',
        '🤔', 'tng-data-conv-uuid-0001', 'tng-picard-account-uuid',
        '$tng-data-react-002:ba_TNG.local-signal.localhost',
        1712100000000, '🤔');
