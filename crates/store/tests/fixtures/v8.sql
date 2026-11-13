-- Schema of eagle store v8 (before profile and groups), with a little user data.
CREATE TABLE tracks(id TEXT PRIMARY KEY,json TEXT NOT NULL);
CREATE TABLE albums(id TEXT PRIMARY KEY,json TEXT NOT NULL);
CREATE TABLE artists(id TEXT PRIMARY KEY,json TEXT NOT NULL);
CREATE TABLE playlists(id TEXT PRIMARY KEY,json TEXT NOT NULL);
CREATE TABLE likes(id TEXT PRIMARY KEY);
CREATE TABLE cache(id TEXT,quality TEXT,path TEXT NOT NULL,size INTEGER NOT NULL,access INTEGER NOT NULL,pinned INTEGER NOT NULL,PRIMARY KEY(id,quality));
CREATE TABLE colors(id TEXT PRIMARY KEY,json TEXT NOT NULL);
CREATE TABLE settings(key TEXT PRIMARY KEY,value TEXT NOT NULL);
CREATE TABLE playback(account TEXT PRIMARY KEY,json TEXT NOT NULL);
CREATE TABLE downloads(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            account TEXT NOT NULL,
            track TEXT NOT NULL,
            quality TEXT NOT NULL CHECK(quality IN ('standard','high')),
            state TEXT NOT NULL CHECK(state IN ('queued','running','paused','cancelled','failed','audio_ready','saved')),
            revision INTEGER NOT NULL DEFAULT 0,
            error TEXT,
            UNIQUE(account,track,quality)
        );
CREATE INDEX downloads_pending ON downloads(account,state,id);
CREATE TABLE history(id INTEGER PRIMARY KEY AUTOINCREMENT,account TEXT NOT NULL,played_at INTEGER NOT NULL,json TEXT NOT NULL);
CREATE INDEX history_account ON history(account,id DESC);
CREATE TABLE access(account TEXT NOT NULL,track TEXT NOT NULL,state TEXT NOT NULL CHECK(state IN ('unknown','confirmed','revoked')),reason TEXT,last_checked INTEGER NOT NULL,PRIMARY KEY(account,track));
CREATE TABLE play_outbox(id TEXT PRIMARY KEY,account TEXT NOT NULL,track TEXT NOT NULL,played_at INTEGER NOT NULL,state TEXT NOT NULL CHECK(state IN ('pending','uncertain','confirmed')));
CREATE TABLE offline_owners(account TEXT NOT NULL,track TEXT NOT NULL,PRIMARY KEY(account,track));
CREATE TABLE radio_play_intents(event_id TEXT PRIMARY KEY REFERENCES play_outbox(id), account TEXT NOT NULL, session_id TEXT NOT NULL, batch_id TEXT, occurrence_id INTEGER NOT NULL CHECK(occurrence_id > 0));
CREATE INDEX radio_play_intents_account ON radio_play_intents(account,event_id);

INSERT INTO tracks VALUES('yandex:1','{"id":"yandex:1","title":"Song 1","artists":[],"album":null,"album_title":null,"duration_ms":180000,"cover":null,"availability":"Available"}'),('yandex:2','{"id":"yandex:2","title":"Song 2","artists":[],"album":null,"album_title":null,"duration_ms":180000,"cover":null,"availability":"Available"}');
INSERT INTO likes VALUES('yandex:1'),('yandex:2');
INSERT INTO playlists VALUES('yandex:1_3','{"id":"yandex:1_3","title":"Mix","tracks":["yandex:1"]}');
INSERT INTO settings VALUES('account_uid:yandex','42'),('theme','dark');
INSERT INTO playback VALUES('42','{"entries":[],"order":[],"current":null,"position_ms":4200,"shuffle":false,"repeat":0}');
INSERT INTO history(account,played_at,json) VALUES('42',100,'{"id":"yandex:1","title":"Song 1","artists":[],"album":null,"album_title":null,"duration_ms":180000,"cover":null,"availability":"Available"}');
INSERT INTO downloads(account,track,quality,state) VALUES('42','yandex:1','high','saved');
INSERT INTO offline_owners VALUES('42','yandex:1');
PRAGMA user_version=8;
