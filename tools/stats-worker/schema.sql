-- One row per request: when it came, the installation's random name, and its lines as JSON.
CREATE TABLE IF NOT EXISTS uploads (
	received INTEGER NOT NULL,
	install TEXT NOT NULL,
	lines TEXT NOT NULL
);
