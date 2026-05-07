-- anthropic_accounts
DROP TABLE IF EXISTS `anthropic_accounts`;
CREATE TABLE anthropic_accounts (
    account_uuid    VARCHAR(64)  NOT NULL,
    email           VARCHAR(320),
    full_name       VARCHAR(255),
    raw_json        JSON         NOT NULL,
    source          VARCHAR(16)  NOT NULL DEFAULT 'export',
    first_seen_at   VARCHAR(40)  NOT NULL,
    last_seen_at    VARCHAR(40)  NOT NULL,
    PRIMARY KEY (account_uuid)
);

INSERT INTO `anthropic_accounts` (`account_uuid`, `email`, `full_name`, `raw_json`, `source`, `first_seen_at`, `last_seen_at`) VALUES ('00000001-1701-4d00-8000-000000000001', 'jlpicard@enterprise.starfleet.test', 'Jean-Luc Picard', '{"email_address":"jlpicard@enterprise.starfleet.test","full_name":"Jean-Luc Picard","uuid":"00000001-1701-4d00-8000-000000000001"}', 'api', '2369-04-15T00:00:00+00:00', '2369-04-15T00:00:00+00:00');
INSERT INTO `anthropic_accounts` (`account_uuid`, `email`, `full_name`, `raw_json`, `source`, `first_seen_at`, `last_seen_at`) VALUES ('00000002-1701-4d00-8000-000000000002', 'lt.laforge@enterprise.starfleet.test', 'Geordi La Forge', '{"email_address":"lt.laforge@enterprise.starfleet.test","full_name":"Geordi La Forge","settings":{},"uuid":"00000002-1701-4d00-8000-000000000002","verified_phone_number":null}', 'export', '2369-04-15T00:00:00+00:00', '2369-04-15T00:00:00+00:00');
INSERT INTO `anthropic_accounts` (`account_uuid`, `email`, `full_name`, `raw_json`, `source`, `first_seen_at`, `last_seen_at`) VALUES ('00000003-1701-4d00-8000-000000000003', 'bcrusher@enterprise.starfleet.test', 'Beverly Crusher', '{"email_address":"bcrusher@enterprise.starfleet.test","full_name":"Beverly Crusher","uuid":"00000003-1701-4d00-8000-000000000003"}', 'api', '2369-04-15T00:00:00+00:00', '2369-04-15T00:00:00+00:00');
