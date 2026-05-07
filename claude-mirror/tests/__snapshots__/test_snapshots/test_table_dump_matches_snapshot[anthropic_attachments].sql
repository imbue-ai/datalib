-- anthropic_attachments
DROP TABLE IF EXISTS `anthropic_attachments`;
CREATE TABLE anthropic_attachments (
    message_uuid     VARCHAR(64)  NOT NULL,
    attachment_index INT         NOT NULL,
    kind             VARCHAR(32) NOT NULL,
    raw_json         JSON         NOT NULL,
    source           VARCHAR(16) NOT NULL DEFAULT 'export',
    PRIMARY KEY (message_uuid, attachment_index, kind)
);

INSERT INTO `anthropic_attachments` (`message_uuid`, `attachment_index`, `kind`, `raw_json`, `source`) VALUES ('20000001-1701-4d00-8000-000000020001', 0, 'attachment', '{"extracted_content":"t,plasma_hz,coolant_k\\n0,750.1,2.7\\n1,750.4,2.7\\n2,750.0,2.7\\n3,750.3,2.7\\n","file_name":"conduit-17-telemetry.csv","file_size":482,"file_type":"csv","id":"a0000001-1701-4d00-8000-0000000a0001"}', 'export');
INSERT INTO `anthropic_attachments` (`message_uuid`, `attachment_index`, `kind`, `raw_json`, `source`) VALUES ('40000001-1701-4d00-8000-000000040001', 0, 'file', '{"created_at":"2369-04-15T08:29:55.000000+00:00","file_kind":"image","file_name":"tricorder-readout.png","file_uuid":"f0000001-1701-4d00-8000-0000000f0001","preview_url":"/api/fake/files/f0000001-1701-4d00-8000-0000000f0001/preview","success":true,"thumbnail_url":"/api/fake/files/f0000001-1701-4d00-8000-0000000f0001/thumbnail"}', 'api');
