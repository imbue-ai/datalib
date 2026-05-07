-- anthropic_projects
DROP TABLE IF EXISTS `anthropic_projects`;
CREATE TABLE anthropic_projects (
    account_uuid    VARCHAR(64)  NOT NULL,
    project_uuid    VARCHAR(64)  NOT NULL,
    name            VARCHAR(512),
    description     TEXT,
    is_starter      BOOLEAN,
    created_at      VARCHAR(40),
    updated_at      VARCHAR(40),
    raw_json        JSON         NOT NULL,
    source          VARCHAR(16)  NOT NULL DEFAULT 'export',
    last_seen_at    VARCHAR(40)  NOT NULL,
    PRIMARY KEY (project_uuid)
);

INSERT INTO `anthropic_projects` (`account_uuid`, `project_uuid`, `name`, `description`, `is_starter`, `created_at`, `updated_at`, `raw_json`, `source`, `last_seen_at`) VALUES ('00000001-1701-4d00-8000-000000000001', '00000010-1701-4d00-8000-000000000010', 'Holodeck Program Library', 'Notes and prompt templates for designing safe holodeck programs. Used as a sandbox for testing new historical-simulation scenarios before deployment to the recreation deck.', 0, '2369-04-12T08:00:00.000000+00:00', '2369-04-15T17:30:00.000000+00:00', '{"created_at":"2369-04-12T08:00:00.000000+00:00","creator":{"full_name":"Jean-Luc Picard","uuid":"00000001-1701-4d00-8000-000000000001"},"description":"Notes and prompt templates for designing safe holodeck programs. Used as a sandbox for testing new historical-simulation scenarios before deployment to the recreation deck.","docs":[{"content":"Holodeck safety protocols must remain engaged at all times. Emergency override: Computer, end program.","filename":"safety-protocols.md","uuid":"000000d0-1701-4d00-8000-0000000000d0"}],"is_private":false,"is_starter_project":false,"name":"Holodeck Program Library","prompt_template":"You are an expert holodeck program designer. Always include safety overrides.","updated_at":"2369-04-15T17:30:00.000000+00:00","uuid":"00000010-1701-4d00-8000-000000000010"}', 'export', '2369-04-15T00:00:00+00:00');
