-- openai_conversations
DROP TABLE IF EXISTS `openai_conversations`;
CREATE TABLE openai_conversations (
    account_id          VARCHAR(64),
    conversation_id     VARCHAR(64)  NOT NULL,
    title               VARCHAR(1024),
    create_time         VARCHAR(40),
    update_time         VARCHAR(40),
    current_node        VARCHAR(64),
    default_model_slug  VARCHAR(128),
    gizmo_id            VARCHAR(128),
    gizmo_type          VARCHAR(64),
    is_archived         BOOLEAN,
    is_starred          BOOLEAN,
    raw_json            JSON         NOT NULL,
    source              VARCHAR(16)  NOT NULL DEFAULT 'api',
    last_seen_at        VARCHAR(40)  NOT NULL,
    PRIMARY KEY (conversation_id)
);

INSERT INTO `openai_conversations` (`account_id`, `conversation_id`, `title`, `create_time`, `update_time`, `current_node`, `default_model_slug`, `gizmo_id`, `gizmo_type`, `is_archived`, `is_starred`, `raw_json`, `source`, `last_seen_at`) VALUES ('user-FAKE0DATAANDROID0POSITRONIC1', '68fa0001-fake-7000-8000-positronic0001', 'Sonnet on a Cat Named Spot', '2370-10-24T08:00:00.000000+00:00', '2370-10-24T08:05:00.000000+00:00', 'msg-fake-spot-0003', 'gpt-5', NULL, NULL, 0, 1, '{"conversation_id":"68fa0001-fake-7000-8000-positronic0001","create_time":12648384000,"current_node":"msg-fake-spot-0003","default_model_slug":"gpt-5","is_archived":false,"is_starred":true,"title":"Sonnet on a Cat Named Spot","update_time":12648384300}', 'api', '2369-04-15T00:00:00+00:00');
INSERT INTO `openai_conversations` (`account_id`, `conversation_id`, `title`, `create_time`, `update_time`, `current_node`, `default_model_slug`, `gizmo_id`, `gizmo_type`, `is_archived`, `is_starred`, `raw_json`, `source`, `last_seen_at`) VALUES ('user-FAKE0DATAANDROID0POSITRONIC1', '68fa0002-fake-7000-8000-positronic0002', 'Polynomial Fit for Sensor Calibration', '2370-10-25T08:00:00.000000+00:00', '2370-10-25T08:05:20.000000+00:00', 'msg-fake-poly-0003', 'gpt-5', NULL, NULL, 0, 0, '{"conversation_id":"68fa0002-fake-7000-8000-positronic0002","create_time":12648470400,"current_node":"msg-fake-poly-0003","default_model_slug":"gpt-5","is_archived":false,"is_starred":false,"title":"Polynomial Fit for Sensor Calibration","update_time":12648470720}', 'api', '2369-04-15T00:00:00+00:00');
