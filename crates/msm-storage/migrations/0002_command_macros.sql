-- Schema v2: user command macros (CMD-02). Never executed as OS shell commands; each line is a
-- validated Minecraft console command template.

CREATE TABLE command_macros (
    id          TEXT PRIMARY KEY NOT NULL,
    instance_id TEXT REFERENCES instances (id) ON DELETE CASCADE,
    name        TEXT NOT NULL,
    def_json    TEXT NOT NULL CHECK (json_valid(def_json)),
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
) STRICT;

CREATE INDEX command_macros_instance ON command_macros (instance_id);
