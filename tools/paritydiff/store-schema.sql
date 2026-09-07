CREATE TABLE counters (
	    key TEXT PRIMARY KEY,
	    ul  INTEGER NOT NULL DEFAULT 0,
	    dl  INTEGER NOT NULL DEFAULT 0
	);

CREATE TABLE jobs (
	    id             TEXT PRIMARY KEY,
	    type           TEXT NOT NULL,
	    state          TEXT NOT NULL,
	    info_hash      TEXT NOT NULL DEFAULT '',
	    params         TEXT NOT NULL DEFAULT '',
	    progress_bytes INTEGER NOT NULL DEFAULT 0,
	    total_bytes    INTEGER NOT NULL DEFAULT 0,
	    error          TEXT NOT NULL DEFAULT '',
	    created_at     INTEGER NOT NULL DEFAULT 0,
	    updated_at     INTEGER NOT NULL DEFAULT 0
	);

CREATE TABLE meta (
	    key   TEXT PRIMARY KEY,
	    value TEXT NOT NULL
	);

CREATE TABLE tag_registry (
	    name TEXT PRIMARY KEY
	);

CREATE TABLE torrents (
    info_hash        TEXT PRIMARY KEY,
    session          TEXT NOT NULL,
    torrent          BLOB NOT NULL,
    save_path        TEXT NOT NULL DEFAULT '',
    category         TEXT NOT NULL DEFAULT '',
    added_time       REAL NOT NULL DEFAULT 0,
    completed_time   REAL NOT NULL DEFAULT 0,
    total_uploaded   INTEGER NOT NULL DEFAULT 0,
    total_downloaded INTEGER NOT NULL DEFAULT 0
, paused INTEGER NOT NULL DEFAULT 0, tags TEXT NOT NULL DEFAULT '', content_folder INTEGER NOT NULL DEFAULT -1, pinned INTEGER NOT NULL DEFAULT 0, seeding_time INTEGER NOT NULL DEFAULT 0);

CREATE INDEX idx_torrents_category ON torrents(category, session, save_path);

CREATE INDEX idx_torrents_session ON torrents(session);

CREATE INDEX jobs_state_idx ON jobs(state);

CREATE INDEX jobs_torrent_idx ON jobs(info_hash);

-- user_version = 7
