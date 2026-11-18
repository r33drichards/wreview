CREATE TABLE IF NOT EXISTS agent_sessions (
  session_id VARBINARY(255) PRIMARY KEY,
  fence BIGINT UNSIGNED NOT NULL DEFAULT 0,
  latest_request VARBINARY(255) NULL
) ENGINE=InnoDB;
CREATE TABLE IF NOT EXISTS agent_checkpoints (
  session_id VARBINARY(255) NOT NULL,
  request_id VARBINARY(255) NOT NULL,
  checkpoint LONGBLOB NOT NULL,
  fence BIGINT UNSIGNED NOT NULL,
  PRIMARY KEY (session_id, request_id),
  FOREIGN KEY (session_id) REFERENCES agent_sessions(session_id)
) ENGINE=InnoDB;
