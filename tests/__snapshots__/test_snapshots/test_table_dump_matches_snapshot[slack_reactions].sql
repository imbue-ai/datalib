-- slack_reactions
DROP TABLE IF EXISTS `slack_reactions`;
CREATE TABLE slack_reactions (
    uuid          VARCHAR(64)  NOT NULL,
    message_uuid  VARCHAR(64)  NOT NULL,
    name          VARCHAR(128) NOT NULL,
    user_id       VARCHAR(64)  NOT NULL,
    last_seen_at  VARCHAR(40)  NOT NULL,
    PRIMARY KEY (uuid)
);

INSERT INTO `slack_reactions` (`uuid`, `message_uuid`, `name`, `user_id`, `last_seen_at`) VALUES ('0b06ef2e-5703-5139-94e7-7f421a72c2cf', '70c76bbb-e431-5ab9-9118-61407fbccf09', 'robot_face', 'U_DATA', '2369-04-15T00:00:00+00:00');
INSERT INTO `slack_reactions` (`uuid`, `message_uuid`, `name`, `user_id`, `last_seen_at`) VALUES ('6f644f1c-4909-5b46-ae24-0a4576cadd66', '70c76bbb-e431-5ab9-9118-61407fbccf09', 'thumbsup', 'U_PICARD', '2369-04-15T00:00:00+00:00');
INSERT INTO `slack_reactions` (`uuid`, `message_uuid`, `name`, `user_id`, `last_seen_at`) VALUES ('e446eea4-8bd0-542d-925d-f2bfb889dab7', '70c76bbb-e431-5ab9-9118-61407fbccf09', 'thumbsup', 'U_RIKER', '2369-04-15T00:00:00+00:00');
INSERT INTO `slack_reactions` (`uuid`, `message_uuid`, `name`, `user_id`, `last_seen_at`) VALUES ('ff2324a8-4937-5070-aa99-e41e01b0b9dd', 'c26c0b71-66a5-535c-9b46-e64351034dc6', 'joy', 'U_RIKER', '2369-04-15T00:00:00+00:00');
