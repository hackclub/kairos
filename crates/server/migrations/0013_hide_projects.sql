-- While set, a project starts hidden from the public the first time Kairos sees it.
ALTER TABLE users ADD COLUMN hide_projects BOOLEAN NOT NULL DEFAULT FALSE;
