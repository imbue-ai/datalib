-- openai_accounts
DROP TABLE IF EXISTS `openai_accounts`;
CREATE TABLE openai_accounts (
    account_id      VARCHAR(64)  NOT NULL,
    email           VARCHAR(320),
    name            VARCHAR(255),
    raw_json        JSON         NOT NULL,
    source          VARCHAR(16)  NOT NULL DEFAULT 'api',
    first_seen_at   VARCHAR(40)  NOT NULL,
    last_seen_at    VARCHAR(40)  NOT NULL,
    PRIMARY KEY (account_id)
);

INSERT INTO `openai_accounts` (`account_id`, `email`, `name`, `raw_json`, `source`, `first_seen_at`, `last_seen_at`) VALUES ('user-FAKE0DATAANDROID0POSITRONIC1', 'data@enterprise.starfleet.test', 'Lt. Cmdr. Data', '{"country":"Federation","email":"data@enterprise.starfleet.test","first_name":"Data","id":"user-FAKE0DATAANDROID0POSITRONIC1","name":"Lt. Cmdr. Data","object":"user","orgs":{"data":[{"id":"org-FAKE0ENTERPRISE0OPS0CRW1","name":"USS Enterprise NCC-1701-D","object":"organization","title":"Operations Officer"}]},"phone_number":null,"picture":"https://avatars.test/data.png"}', 'api', '2369-04-15T00:00:00+00:00', '2369-04-15T00:00:00+00:00');
