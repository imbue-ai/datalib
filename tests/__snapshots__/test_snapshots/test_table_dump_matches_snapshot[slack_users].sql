-- slack_users
DROP TABLE IF EXISTS `slack_users`;
CREATE TABLE slack_users (
    team_id       VARCHAR(64)  NOT NULL,
    user_id       VARCHAR(64)  NOT NULL,
    name          VARCHAR(255),
    real_name     VARCHAR(255),
    display_name  VARCHAR(255),
    title         VARCHAR(255),
    deleted       BOOLEAN,
    raw_json      JSON         NOT NULL,
    last_seen_at  VARCHAR(40)  NOT NULL,
    PRIMARY KEY (user_id)
);

INSERT INTO `slack_users` (`team_id`, `user_id`, `name`, `real_name`, `display_name`, `title`, `deleted`, `raw_json`, `last_seen_at`) VALUES ('T_NCC1701D', 'USLACKBOT', 'slackbot', 'Slackbot', 'Slackbot', NULL, 0, '{"deleted":false,"id":"USLACKBOT","name":"slackbot","profile":{"display_name":"Slackbot","real_name":"Slackbot"},"real_name":"Slackbot","team_id":"T_NCC1701D"}', '2369-04-15T00:00:00+00:00');
INSERT INTO `slack_users` (`team_id`, `user_id`, `name`, `real_name`, `display_name`, `title`, `deleted`, `raw_json`, `last_seen_at`) VALUES ('T_NCC1701D', 'U_DATA', 'data', 'Lt. Cmdr. Data', 'Data', 'Operations Officer', 0, '{"deleted":false,"id":"U_DATA","name":"data","profile":{"display_name":"Data","real_name":"Lt. Cmdr. Data","title":"Operations Officer"},"real_name":"Lt. Cmdr. Data","team_id":"T_NCC1701D"}', '2369-04-15T00:00:00+00:00');
INSERT INTO `slack_users` (`team_id`, `user_id`, `name`, `real_name`, `display_name`, `title`, `deleted`, `raw_json`, `last_seen_at`) VALUES ('T_NCC1701D', 'U_PICARD', 'picard', 'Jean-Luc Picard', 'Captain Picard', 'Captain', 0, '{"deleted":false,"id":"U_PICARD","name":"picard","profile":{"display_name":"Captain Picard","real_name":"Jean-Luc Picard","title":"Captain"},"real_name":"Jean-Luc Picard","team_id":"T_NCC1701D"}', '2369-04-15T00:00:00+00:00');
INSERT INTO `slack_users` (`team_id`, `user_id`, `name`, `real_name`, `display_name`, `title`, `deleted`, `raw_json`, `last_seen_at`) VALUES ('T_NCC1701D', 'U_RIKER', 'riker', 'William T. Riker', 'Number One', 'First Officer', 0, '{"deleted":false,"id":"U_RIKER","name":"riker","profile":{"display_name":"Number One","real_name":"William T. Riker","title":"First Officer"},"real_name":"William T. Riker","team_id":"T_NCC1701D"}', '2369-04-15T00:00:00+00:00');
INSERT INTO `slack_users` (`team_id`, `user_id`, `name`, `real_name`, `display_name`, `title`, `deleted`, `raw_json`, `last_seen_at`) VALUES ('T_NCC1701D', 'U_WORF', 'worf', 'Worf', 'Worf', 'Chief of Security', 0, '{"deleted":false,"id":"U_WORF","name":"worf","profile":{"display_name":"Worf","real_name":"Worf","title":"Chief of Security"},"real_name":"Worf","team_id":"T_NCC1701D"}', '2369-04-15T00:00:00+00:00');
