-- slack_workspaces
DROP TABLE IF EXISTS `slack_workspaces`;
CREATE TABLE slack_workspaces (
    team_id       VARCHAR(64)  NOT NULL,
    team_name     VARCHAR(255),
    team_url      VARCHAR(512),
    self_user_id  VARCHAR(64),
    raw_json      JSON         NOT NULL,
    first_seen_at VARCHAR(40)  NOT NULL,
    last_seen_at  VARCHAR(40)  NOT NULL,
    PRIMARY KEY (team_id)
);

INSERT INTO `slack_workspaces` (`team_id`, `team_name`, `team_url`, `self_user_id`, `raw_json`, `first_seen_at`, `last_seen_at`) VALUES ('T_NCC1701D', 'USS Enterprise NCC-1701-D', 'https://enterprise-d.slack.com/', 'U_PICARD', '{"is_enterprise_install":false,"ok":true,"team":"USS Enterprise NCC-1701-D","team_id":"T_NCC1701D","url":"https://enterprise-d.slack.com/","user":"picard","user_id":"U_PICARD"}', '2369-04-15T00:00:00+00:00', '2369-04-15T00:00:00+00:00');
