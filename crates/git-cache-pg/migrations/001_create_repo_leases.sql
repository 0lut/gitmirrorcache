CREATE TABLE IF NOT EXISTS repo_leases (
    repo_key    TEXT PRIMARY KEY,
    holder      TEXT NOT NULL,
    acquired_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at  TIMESTAMPTZ NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_repo_leases_expires_at ON repo_leases (expires_at);
