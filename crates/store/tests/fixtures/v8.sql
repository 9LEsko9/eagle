PRAGMA foreign_keys=OFF;
BEGIN TRANSACTION;
CREATE TABLE tracks(id TEXT PRIMARY KEY,json TEXT NOT NULL);
INSERT INTO tracks VALUES('yandex:1','{"id":"yandex:1","title":"First","artists":[{"id":"yandex:a1","name":"Artist","cover":null}],"album":"yandex:al1","album_title":"Album","duration_ms":200000,"cover":null,"availability":"Available"}');
INSERT INTO tracks VALUES('yandex:2','{"id":"yandex:2","title":"Second","artists":[{"id":"yandex:a1","name":"Artist","cover":null}],"album":"yandex:al1","album_title":"Album","duration_ms":200000,"cover":null,"availability":"Available"}');
CREATE TABLE albums(id TEXT PRIMARY KEY,json TEXT NOT NULL);
CREATE TABLE artists(id TEXT PRIMARY KEY,json TEXT NOT NULL);
INSERT INTO artists VALUES('yandex:a1','{"id":"yandex:a1","name":"Artist","cover":null}');
CREATE TABLE playlists(id TEXT PRIMARY KEY,json TEXT NOT NULL);
INSERT INTO playlists VALUES('yandex:pl1','{"id":"yandex:pl1","title":"Mix","tracks":[{"id":"yandex:1","title":"First","artists":[{"id":"yandex:a1","name":"Artist","cover":null}],"album":"yandex:al1","album_title":"Album","duration_ms":200000,"cover":null,"availability":"Available"},{"id":"yandex:2","title":"Second","artists":[{"id":"yandex:a1","name":"Artist","cover":null}],"album":"yandex:al1","album_title":"Album","duration_ms":200000,"cover":null,"availability":"Available"}],"cover":null}');
CREATE TABLE likes(id TEXT PRIMARY KEY);
INSERT INTO likes VALUES('yandex:3');
INSERT INTO likes VALUES('yandex:1');
INSERT INTO likes VALUES('yandex:2');
CREATE TABLE cache(id TEXT,quality TEXT,path TEXT NOT NULL,size INTEGER NOT NULL,access INTEGER NOT NULL,pinned INTEGER NOT NULL,PRIMARY KEY(id,quality));
CREATE TABLE colors(id TEXT PRIMARY KEY,json TEXT NOT NULL);
CREATE TABLE settings(key TEXT PRIMARY KEY,value TEXT NOT NULL);
INSERT INTO settings VALUES('account_uid:yandex','u1');
INSERT INTO settings VALUES('theme','dark');
CREATE TABLE playback(account TEXT PRIMARY KEY,json TEXT NOT NULL);
INSERT INTO playback VALUES('u1','{"entries":[{"id":1,"track":{"id":"yandex:1","title":"First","artists":[{"id":"yandex:a1","name":"Artist","cover":null}],"album":"yandex:al1","album_title":"Album","duration_ms":200000,"cover":null,"availability":"Available"},"radio":null},{"id":2,"track":{"id":"yandex:2","title":"Second","artists":[{"id":"yandex:a1","name":"Artist","cover":null}],"album":"yandex:al1","album_title":"Album","duration_ms":200000,"cover":null,"availability":"Available"},"radio":null}],"order":[1,2],"current":1,"position_ms":4200,"shuffle":false,"repeat":0}');
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
INSERT INTO downloads VALUES(1,'u1','yandex:1','standard','saved',4,NULL);
CREATE TABLE history(id INTEGER PRIMARY KEY AUTOINCREMENT,account TEXT NOT NULL,played_at INTEGER NOT NULL,json TEXT NOT NULL);
INSERT INTO history VALUES(1,'u1',1794588771,'{"id":"yandex:1","title":"First","artists":[{"id":"yandex:a1","name":"Artist","cover":null}],"album":"yandex:al1","album_title":"Album","duration_ms":200000,"cover":null,"availability":"Available"}');
INSERT INTO history VALUES(2,'u1',1794588771,'{"id":"yandex:2","title":"Second","artists":[{"id":"yandex:a1","name":"Artist","cover":null}],"album":"yandex:al1","album_title":"Album","duration_ms":200000,"cover":null,"availability":"Available"}');
CREATE TABLE access(account TEXT NOT NULL,track TEXT NOT NULL,state TEXT NOT NULL CHECK(state IN ('unknown','confirmed','revoked')),reason TEXT,last_checked INTEGER NOT NULL,PRIMARY KEY(account,track));
INSERT INTO access VALUES('u1','yandex:1','confirmed',NULL,1794588771);
CREATE TABLE play_outbox(id TEXT PRIMARY KEY,account TEXT NOT NULL,track TEXT NOT NULL,played_at INTEGER NOT NULL,state TEXT NOT NULL CHECK(state IN ('pending','uncertain','confirmed')));
INSERT INTO play_outbox VALUES('026f6aab87592f31541db5dbb89787df','u1','yandex:1',1794588771,'pending');
INSERT INTO play_outbox VALUES('19934714a9dbee6966fa267771ebd435','u1','yandex:2',1794588771,'pending');
CREATE TABLE offline_owners(account TEXT NOT NULL,track TEXT NOT NULL,PRIMARY KEY(account,track));
INSERT INTO offline_owners VALUES('u1','yandex:1');
CREATE TABLE radio_play_intents(event_id TEXT PRIMARY KEY REFERENCES play_outbox(id), account TEXT NOT NULL, session_id TEXT NOT NULL, batch_id TEXT, occurrence_id INTEGER NOT NULL CHECK(occurrence_id > 0));
PRAGMA writable_schema=ON;
CREATE TABLE IF NOT EXISTS sqlite_sequence(name,seq);
DELETE FROM sqlite_sequence;
INSERT INTO sqlite_sequence VALUES('downloads',1);
INSERT INTO sqlite_sequence VALUES('history',2);
CREATE INDEX downloads_pending ON downloads(account,state,id);
CREATE INDEX history_account ON history(account,id DESC);
CREATE INDEX radio_play_intents_account ON radio_play_intents(account,event_id);
PRAGMA writable_schema=OFF;
COMMIT;
PRAGMA user_version=8;
