-- WebDAV file servers for the Files explorer and compose attachments. URL and password are
-- stored encrypted, like calendar_import_sources.
CREATE TABLE IF NOT EXISTS webdav_file_servers (
  id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
  user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  display_name TEXT NOT NULL,
  url TEXT NOT NULL,
  username TEXT NOT NULL,
  password TEXT NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_webdav_file_servers_user ON webdav_file_servers(user_id, created_at);
