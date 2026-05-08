-- slack_channels
DROP TABLE IF EXISTS `slack_channels`;
CREATE TABLE slack_channels (
    team_id       VARCHAR(64)  NOT NULL,
    channel_id    VARCHAR(64)  NOT NULL,
    name          VARCHAR(255),
    is_private    BOOLEAN,
    is_archived   BOOLEAN,
    topic         VARCHAR(1024),
    purpose       VARCHAR(1024),
    raw_json      JSON         NOT NULL,
    last_seen_at  VARCHAR(40)  NOT NULL,
    PRIMARY KEY (channel_id)
);

INSERT INTO `slack_channels` (`team_id`, `channel_id`, `name`, `is_private`, `is_archived`, `topic`, `purpose`, `raw_json`, `last_seen_at`) VALUES ('T_NCC1701D', 'C_BRIDGE', 'bridge', 0, 0, 'Main bridge ops', 'Coordinate bridge crew.', '{"creator":"U_PICARD","id":"C_BRIDGE","is_archived":false,"is_channel":true,"is_general":true,"is_private":false,"name":"bridge","name_normalized":"bridge","purpose":{"value":"Coordinate bridge crew."},"topic":{"value":"Main bridge ops"}}', '2369-04-15T00:00:00+00:00');
INSERT INTO `slack_channels` (`team_id`, `channel_id`, `name`, `is_private`, `is_archived`, `topic`, `purpose`, `raw_json`, `last_seen_at`) VALUES ('T_NCC1701D', 'C_ENG', 'engineering', 0, 0, 'Warp core', 'Engineering chatter.', '{"creator":"U_RIKER","id":"C_ENG","is_archived":false,"is_channel":true,"is_general":false,"is_private":false,"name":"engineering","name_normalized":"engineering","purpose":{"value":"Engineering chatter."},"topic":{"value":"Warp core"}}', '2369-04-15T00:00:00+00:00');
INSERT INTO `slack_channels` (`team_id`, `channel_id`, `name`, `is_private`, `is_archived`, `topic`, `purpose`, `raw_json`, `last_seen_at`) VALUES ('T_NCC1701D', 'C_TENFWD', 'ten-forward', 0, 0, 'Off-duty lounge', 'Drinks with Guinan.', '{"creator":"U_RIKER","id":"C_TENFWD","is_archived":false,"is_channel":true,"is_general":false,"is_private":false,"name":"ten-forward","name_normalized":"ten-forward","purpose":{"value":"Drinks with Guinan."},"topic":{"value":"Off-duty lounge"}}', '2369-04-15T00:00:00+00:00');
