//! LMS `library.db` SQLite sink trait and `rusqlite` implementation.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Mutex;

use rusqlite::types::Value as SqlValue;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Transaction};
use spotty_types::{Result, SpottyError};

use crate::track_prep::LmsTrackRow;

/// Storage sink for persisting Spotify library entities into LMS `library.db`.
pub trait LibrarySink: Send + Sync {
    /// Ensures the staging `online_tracks` table exists and clears prior scan rows.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Database`] if SQLite table creation or cleanup fails.
    fn init_online_tracks_table(&mut self) -> Result<()>;

    /// Upserts prepared track rows and records their URIs in `online_tracks`.
    ///
    /// Returns the number of track rows processed.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Database`] if the SQLite transaction or statements
    /// fail.
    fn store_tracks(&mut self, rows: &[LmsTrackRow]) -> Result<usize>;

    /// Upserts a contributor record by `extid` (or `name` when `extid` is empty).
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Database`] if querying or mutating `contributors`
    /// fails.
    fn upsert_contributor(&mut self, name: &str, normalized_sort: &str, extid: &str) -> Result<()>;

    /// Removes all Spotify playlist rows (`url LIKE 'spotify:playlist:%'`) and
    /// their associated `playlist_track` entries.
    ///
    /// Returns the number of deleted playlist rows from `tracks`.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Database`] if the SQLite delete transaction fails.
    fn purge_spotify_playlists(&mut self) -> Result<usize>;

    /// Upserts a Spotify playlist with `"Spotify: {name}"` title prefix and
    /// `content_type = "ssp"`, replacing its ordered `playlist_track` rows.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Database`] if the SQLite upsert transaction fails.
    fn upsert_playlist(
        &mut self,
        uri: &str,
        name: &str,
        cover: &str,
        track_uris: &[String],
    ) -> Result<()>;

    /// Deletes `spotify:track:%` rows from `tracks` that were not recorded in
    /// `online_tracks` during the current scan.
    ///
    /// Returns the number of pruned track rows.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Database`] if the SQLite delete transaction fails.
    fn delete_removed_tracks(&mut self) -> Result<usize>;
}

/// [`LibrarySink`] backed by a [`rusqlite::Connection`] to LMS `library.db`.
#[derive(Debug)]
pub struct SqliteLibrarySink {
    conn: Mutex<Connection>,
}

impl SqliteLibrarySink {
    /// Wraps an existing [`Connection`] and ensures compatible LMS tables exist.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Database`] if schema initialization fails.
    pub fn new(conn: Connection) -> Result<Self> {
        ensure_schema(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Opens an in-memory SQLite database initialized with LMS-compatible tables.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Database`] if opening or initializing SQLite fails.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().map_err(db_err)?;
        Self::new(conn)
    }

    /// Opens a file-backed SQLite database at `path` and ensures LMS tables exist.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Database`] if opening or initializing SQLite fails.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path).map_err(db_err)?;
        Self::new(conn)
    }

    /// Runs a read-only closure with shared access to the [`Connection`].
    ///
    /// # Panics
    ///
    /// Panics if the internal SQLite connection mutex was poisoned by a prior
    /// panic.
    pub fn with_connection<R>(&self, f: impl FnOnce(&Connection) -> R) -> R {
        let guard = self
            .conn
            .lock()
            .expect("sqlite connection mutex is never poisoned");
        f(&guard)
    }

    /// Runs a closure with exclusive mutable access to the [`Connection`].
    ///
    /// # Panics
    ///
    /// Panics if the internal SQLite connection mutex was poisoned by a prior
    /// panic.
    pub fn with_mut_connection<R>(&mut self, f: impl FnOnce(&mut Connection) -> R) -> R {
        let mut guard = self
            .conn
            .lock()
            .expect("sqlite connection mutex is never poisoned");
        f(&mut guard)
    }

    /// Consumes this sink and returns the underlying [`Connection`].
    ///
    /// # Panics
    ///
    /// Panics if the internal SQLite connection mutex was poisoned by a prior
    /// panic.
    #[must_use]
    pub fn into_connection(self) -> Connection {
        self.conn
            .into_inner()
            .expect("sqlite connection mutex is never poisoned")
    }
}

impl LibrarySink for SqliteLibrarySink {
    fn init_online_tracks_table(&mut self) -> Result<()> {
        self.with_mut_connection(|conn| {
            let tx = conn.transaction().map_err(db_err)?;
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS online_tracks (
                     url TEXT PRIMARY KEY
                 );
                 DELETE FROM online_tracks;",
            )
            .map_err(db_err)?;
            tx.commit().map_err(db_err)?;
            Ok(())
        })
    }

    fn store_tracks(&mut self, rows: &[LmsTrackRow]) -> Result<usize> {
        if rows.is_empty() {
            return Ok(0);
        }

        self.with_mut_connection(|conn| {
            let tx = conn.transaction().map_err(db_err)?;
            let cols = table_columns(&tx, "tracks")?;

            for row in rows {
                let fields = track_row_fields(row);
                upsert_track_fields(&tx, &cols, &row.url, &fields)?;
                tx.execute(
                    "INSERT OR REPLACE INTO online_tracks (url) VALUES (?1)",
                    params![row.url],
                )
                .map_err(db_err)?;
            }

            tx.commit().map_err(db_err)?;
            Ok(rows.len())
        })
    }

    fn upsert_contributor(&mut self, name: &str, normalized_sort: &str, extid: &str) -> Result<()> {
        self.with_mut_connection(|conn| {
            let tx = conn.transaction().map_err(db_err)?;
            let cols = table_columns(&tx, "contributors")?;

            let existing_id: Option<i64> = if !extid.is_empty() && cols.contains("extid") {
                tx.query_row(
                    "SELECT id FROM contributors WHERE extid = ?1 LIMIT 1",
                    params![extid],
                    |r| r.get(0),
                )
                .optional()
                .map_err(db_err)?
            } else {
                tx.query_row(
                    "SELECT id FROM contributors WHERE name = ?1 LIMIT 1",
                    params![name],
                    |r| r.get(0),
                )
                .optional()
                .map_err(db_err)?
            };

            let fields = vec![
                ("name", SqlValue::Text(name.to_owned())),
                ("namesort", SqlValue::Text(normalized_sort.to_owned())),
                ("namesearch", SqlValue::Text(name.to_uppercase())),
                (
                    "extid",
                    if extid.is_empty() {
                        SqlValue::Null
                    } else {
                        SqlValue::Text(extid.to_owned())
                    },
                ),
            ];

            if let Some(id) = existing_id {
                update_by_id(&tx, "contributors", &cols, id, &fields)?;
            } else {
                insert_row(&tx, "contributors", &cols, &fields)?;
            }

            tx.commit().map_err(db_err)?;
            Ok(())
        })
    }

    fn purge_spotify_playlists(&mut self) -> Result<usize> {
        self.with_mut_connection(|conn| {
            let tx = conn.transaction().map_err(db_err)?;
            tx.execute(
                "DELETE FROM playlist_track
                 WHERE playlist IN (SELECT id FROM tracks WHERE url LIKE 'spotify:playlist:%')",
                [],
            )
            .map_err(db_err)?;
            let deleted = tx
                .execute("DELETE FROM tracks WHERE url LIKE 'spotify:playlist:%'", [])
                .map_err(db_err)?;
            tx.commit().map_err(db_err)?;
            Ok(deleted)
        })
    }

    fn upsert_playlist(
        &mut self,
        uri: &str,
        name: &str,
        cover: &str,
        track_uris: &[String],
    ) -> Result<()> {
        self.with_mut_connection(|conn| {
            let tx = conn.transaction().map_err(db_err)?;
            let cols = table_columns(&tx, "tracks")?;
            let prefixed_title = format!("Spotify: {name}");

            let fields = vec![
                ("url", SqlValue::Text(uri.to_owned())),
                ("title", SqlValue::Text(prefixed_title.clone())),
                ("titlesort", SqlValue::Text(prefixed_title.to_uppercase())),
                ("titlesearch", SqlValue::Text(prefixed_title.to_uppercase())),
                ("cover", SqlValue::Text(cover.to_owned())),
                ("audio", SqlValue::Integer(1)),
                ("extid", SqlValue::Text(uri.to_owned())),
                ("content_type", SqlValue::Text(String::from("ssp"))),
            ];

            let playlist_id = upsert_track_fields(&tx, &cols, uri, &fields)?;

            tx.execute(
                "DELETE FROM playlist_track WHERE playlist = ?1",
                params![playlist_id],
            )
            .map_err(db_err)?;

            for (idx, track_uri) in track_uris.iter().enumerate() {
                let pos = i64::try_from(idx).map_err(|err| {
                    SpottyError::Database(format!("playlist index overflow: {err}"))
                })?;
                tx.execute(
                    "INSERT INTO playlist_track (position, playlist, track) VALUES (?1, ?2, ?3)",
                    params![pos, playlist_id, track_uri],
                )
                .map_err(db_err)?;
            }

            tx.commit().map_err(db_err)?;
            Ok(())
        })
    }

    fn delete_removed_tracks(&mut self) -> Result<usize> {
        self.with_mut_connection(|conn| {
            let tx = conn.transaction().map_err(db_err)?;
            let deleted = tx
                .execute(
                    "DELETE FROM tracks
                     WHERE url LIKE 'spotify:track:%'
                       AND url NOT IN (SELECT url FROM online_tracks)",
                    [],
                )
                .map_err(db_err)?;
            tx.commit().map_err(db_err)?;
            Ok(deleted)
        })
    }
}

fn ensure_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS tracks (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             url TEXT NOT NULL UNIQUE,
             title TEXT NOT NULL,
             titlesort TEXT,
             titlesearch TEXT,
             artist TEXT NOT NULL DEFAULT '',
             artist_extid TEXT,
             album TEXT NOT NULL DEFAULT '',
             album_extid TEXT NOT NULL DEFAULT '',
             tracknum INTEGER NOT NULL DEFAULT 0,
             disc INTEGER,
             discc INTEGER,
             secs INTEGER NOT NULL DEFAULT 0,
             year INTEGER,
             cover TEXT NOT NULL DEFAULT '',
             audio INTEGER NOT NULL DEFAULT 1,
             extid TEXT NOT NULL DEFAULT '',
             compilation INTEGER NOT NULL DEFAULT 0,
             release_type TEXT NOT NULL DEFAULT 'ALBUM',
             timestamp INTEGER NOT NULL DEFAULT 0,
             content_type TEXT NOT NULL DEFAULT 'spt',
             genre TEXT NOT NULL DEFAULT 'Spotify',
             account_tag TEXT
         );
         CREATE TABLE IF NOT EXISTS contributors (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             name TEXT NOT NULL,
             namesort TEXT NOT NULL DEFAULT '',
             namesearch TEXT NOT NULL DEFAULT '',
             extid TEXT
         );
         CREATE TABLE IF NOT EXISTS playlist_track (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             position INTEGER NOT NULL,
             playlist INTEGER NOT NULL,
             track TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS online_tracks (
             url TEXT PRIMARY KEY
         );",
    )
    .map_err(db_err)
}

fn track_row_fields(row: &LmsTrackRow) -> Vec<(&'static str, SqlValue)> {
    vec![
        ("url", SqlValue::Text(row.url.clone())),
        ("title", SqlValue::Text(row.title.clone())),
        ("titlesort", SqlValue::Text(row.title.to_uppercase())),
        ("titlesearch", SqlValue::Text(row.title.to_uppercase())),
        ("artist", SqlValue::Text(row.artist.clone())),
        ("artist_extid", opt_text(row.artist_extid.as_deref())),
        ("album", SqlValue::Text(row.album.clone())),
        ("album_extid", SqlValue::Text(row.album_extid.clone())),
        ("tracknum", SqlValue::Integer(i64::from(row.tracknum))),
        ("disc", opt_u32(row.disc)),
        ("discc", opt_u32(row.discc)),
        ("secs", SqlValue::Integer(i64::from(row.secs))),
        ("year", opt_u32(row.year)),
        ("cover", SqlValue::Text(row.cover.clone())),
        ("audio", SqlValue::Integer(1)),
        ("extid", SqlValue::Text(row.extid.clone())),
        ("compilation", SqlValue::Integer(i64::from(row.compilation))),
        (
            "release_type",
            SqlValue::Text(row.release_type.as_lms_str().to_owned()),
        ),
        ("timestamp", SqlValue::Integer(row.timestamp)),
        ("content_type", SqlValue::Text(row.content_type.clone())),
        ("genre", SqlValue::Text(row.genre.clone())),
        ("account_tag", opt_text(row.account_tag.as_deref())),
    ]
}

fn opt_text(value: Option<&str>) -> SqlValue {
    match value {
        Some(text) => SqlValue::Text(text.to_owned()),
        None => SqlValue::Null,
    }
}

fn opt_u32(value: Option<u32>) -> SqlValue {
    match value {
        Some(num) => SqlValue::Integer(i64::from(num)),
        None => SqlValue::Null,
    }
}

fn table_columns(tx: &Transaction<'_>, table: &str) -> Result<HashSet<String>> {
    let pragma = format!("PRAGMA table_info({table})");
    let mut stmt = tx.prepare(&pragma).map_err(db_err)?;
    let mut rows = stmt.query([]).map_err(db_err)?;
    let mut cols = HashSet::new();
    while let Some(row) = rows.next().map_err(db_err)? {
        let name: String = row.get(1).map_err(db_err)?;
        cols.insert(name);
    }
    Ok(cols)
}

fn upsert_track_fields(
    tx: &Transaction<'_>,
    cols: &HashSet<String>,
    url: &str,
    fields: &[(&'static str, SqlValue)],
) -> Result<i64> {
    let existing_id: Option<i64> = tx
        .query_row(
            "SELECT id FROM tracks WHERE url = ?1 LIMIT 1",
            params![url],
            |r| r.get(0),
        )
        .optional()
        .map_err(db_err)?;

    if let Some(id) = existing_id {
        update_by_id(tx, "tracks", cols, id, fields)?;
        Ok(id)
    } else {
        insert_row(tx, "tracks", cols, fields)?;
        Ok(tx.last_insert_rowid())
    }
}

fn update_by_id(
    tx: &Transaction<'_>,
    table: &str,
    cols: &HashSet<String>,
    id: i64,
    fields: &[(&'static str, SqlValue)],
) -> Result<()> {
    let mut assignments = Vec::new();
    let mut values = Vec::new();
    for (col, val) in fields {
        if *col == "url" || !cols.contains(*col) {
            continue;
        }
        let idx = values.len() + 1;
        assignments.push(format!("{col} = ?{idx}"));
        values.push(val.clone());
    }
    if assignments.is_empty() {
        return Ok(());
    }
    let id_idx = values.len() + 1;
    values.push(SqlValue::Integer(id));
    let sql = format!(
        "UPDATE {table} SET {} WHERE id = ?{id_idx}",
        assignments.join(", ")
    );
    tx.execute(&sql, params_from_iter(values)).map_err(db_err)?;
    Ok(())
}

fn insert_row(
    tx: &Transaction<'_>,
    table: &str,
    cols: &HashSet<String>,
    fields: &[(&'static str, SqlValue)],
) -> Result<()> {
    let mut col_names = Vec::new();
    let mut placeholders = Vec::new();
    let mut values = Vec::new();
    for (col, val) in fields {
        if !cols.contains(*col) {
            continue;
        }
        let idx = values.len() + 1;
        col_names.push(*col);
        placeholders.push(format!("?{idx}"));
        values.push(val.clone());
    }
    let sql = format!(
        "INSERT INTO {table} ({}) VALUES ({})",
        col_names.join(", "),
        placeholders.join(", ")
    );
    tx.execute(&sql, params_from_iter(values)).map_err(db_err)?;
    Ok(())
}

fn db_err(err: rusqlite::Error) -> SpottyError {
    SpottyError::Database(err.to_string())
}

#[cfg(test)]
mod tests {
    use spotty_api::{ArtistSummary, MinAlbum, NormalizedTrack};
    use spotty_types::TagCleanupPolicy;

    use super::*;
    use crate::track_prep::{prepare_lms_track, LmsReleaseType};

    fn make_row(id: &str, title: &str) -> LmsTrackRow {
        let mut track = NormalizedTrack::default();
        track.id = id.to_owned();
        track.uri = format!("spotify:track:{id}");
        track.name = title.to_owned();
        track.artists = vec![ArtistSummary::new(
            "art1",
            "spotify:artist:art1",
            "Artist 1",
        )];
        track.album = MinAlbum::new(
            "alb1",
            "spotify:album:alb1",
            "Album 1",
            track.artists.clone(),
            "https://img.example/alb1.jpg",
            "2025-02-01",
            "album",
            Some(String::from("2025-03-01T00:00:00Z")),
        );
        track.duration_ms = 180_000;
        track.track_number = 1;
        track.disc_number = 1;
        prepare_lms_track(
            &track,
            None,
            1,
            8,
            ';',
            TagCleanupPolicy::StripSuffixes,
            None,
        )
    }

    #[test]
    fn store_tracks_upserts_and_delete_removed_tracks_prunes_only_stale_spotify_tracks() {
        let mut sink = SqliteLibrarySink::open_in_memory().expect("in-memory sink opens");

        sink.with_connection(|conn| {
            conn.execute(
                "INSERT INTO tracks (url, title, content_type, extid)
                 VALUES ('file:///music/local.mp3', 'Local Song', 'mp3', 'local-1')",
                [],
            )
            .expect("seed local track");
        });

        // First scan stores track1 and track2.
        sink.init_online_tracks_table()
            .expect("init online_tracks 1");
        let row1 = make_row("t1", "Song One (Remastered)");
        let row2 = make_row("t2", "Song Two");
        let stored = sink.store_tracks(&[row1, row2]).expect("store scan 1");
        assert_eq!(stored, 2, "expected 2 stored rows, got {stored}");

        // Second scan only sees t1 with an updated title; t2 should be pruned.
        sink.init_online_tracks_table()
            .expect("init online_tracks 2");
        let mut row1_updated = make_row("t1", "Song One Updated");
        row1_updated.release_type = LmsReleaseType::Ep;
        sink.store_tracks(&[row1_updated]).expect("store scan 2");

        let pruned = sink.delete_removed_tracks().expect("prune removed tracks");
        assert_eq!(pruned, 1, "expected 1 stale track pruned, got {pruned}");

        let remaining: Vec<(String, String, String)> = sink.with_connection(|conn| {
            let mut stmt = conn
                .prepare("SELECT url, title, release_type FROM tracks ORDER BY url")
                .expect("prepare remaining query");
            let mut rows = stmt.query([]).expect("query remaining");
            let mut out = Vec::new();
            while let Some(r) = rows.next().expect("read row") {
                out.push((
                    r.get(0).expect("url"),
                    r.get(1).expect("title"),
                    r.get(2).expect("release_type"),
                ));
            }
            out
        });

        assert_eq!(
            remaining,
            vec![
                (
                    String::from("file:///music/local.mp3"),
                    String::from("Local Song"),
                    String::from("ALBUM"),
                ),
                (
                    String::from("spotify:track:t1"),
                    String::from("Song One Updated"),
                    String::from("EP"),
                ),
            ],
            "unexpected remaining rows after prune: {remaining:?}"
        );
    }

    #[test]
    fn upsert_contributor_updates_existing_by_extid() {
        let mut sink = SqliteLibrarySink::open_in_memory().expect("in-memory sink opens");
        sink.upsert_contributor("The Band", "BAND, THE", "spotify:artist:band1")
            .expect("insert contributor");
        sink.upsert_contributor("The Band Renamed", "BAND RENAMED", "spotify:artist:band1")
            .expect("update contributor");

        let (count, name, namesort): (i64, String, String) = sink.with_connection(|conn| {
            let count: i64 = conn
                .query_row("SELECT COUNT(*) FROM contributors", [], |r| r.get(0))
                .expect("count contributors");
            let (name, namesort): (String, String) = conn
                .query_row(
                    "SELECT name, namesort FROM contributors WHERE extid = 'spotify:artist:band1'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .expect("fetch contributor");
            (count, name, namesort)
        });
        assert_eq!(
            count, 1,
            "expected single upserted contributor, got {count}"
        );
        assert_eq!(name, "The Band Renamed");
        assert_eq!(namesort, "BAND RENAMED");
    }

    #[test]
    fn upsert_and_purge_spotify_playlists_replaces_ordered_tracks() {
        let mut sink = SqliteLibrarySink::open_in_memory().expect("in-memory sink opens");

        // Seed a non-Spotify playlist that must survive purge_spotify_playlists.
        sink.with_connection(|conn| {
            conn.execute(
                "INSERT INTO tracks (url, title, content_type)
                 VALUES ('file:///playlists/local.m3u', 'Local Playlist', 'm3u')",
                [],
            )
            .expect("seed local playlist");
        });

        let pl_uri = "spotify:playlist:pl1";
        sink.upsert_playlist(
            pl_uri,
            "Chill Vibes",
            "https://img.example/pl1.jpg",
            &[
                String::from("spotify:track:t1"),
                String::from("spotify:track:t2"),
            ],
        )
        .expect("insert playlist");

        // Re-upserting with a different track list replaces playlist_track rows.
        sink.upsert_playlist(
            pl_uri,
            "Chill Vibes v2",
            "https://img.example/pl1_v2.jpg",
            &[String::from("spotify:track:t3")],
        )
        .expect("replace playlist");

        let (title, content_type, cover, pl_tracks): (String, String, String, Vec<(i64, String)>) =
            sink.with_connection(|conn| {
                let (title, content_type, cover): (String, String, String) = conn
                    .query_row(
                        "SELECT title, content_type, cover FROM tracks WHERE url = ?1",
                        params![pl_uri],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .expect("query playlist track row");

                let mut stmt = conn
                    .prepare(
                        "SELECT pt.position, pt.track
                         FROM playlist_track pt
                         JOIN tracks t ON t.id = pt.playlist
                         WHERE t.url = ?1
                         ORDER BY pt.position",
                    )
                    .expect("prepare playlist_track query");
                let mut rows = stmt.query(params![pl_uri]).expect("query playlist_track");
                let mut out = Vec::new();
                while let Some(r) = rows.next().expect("read pt row") {
                    out.push((r.get(0).expect("position"), r.get(1).expect("track")));
                }
                (title, content_type, cover, out)
            });

        assert_eq!(title, "Spotify: Chill Vibes v2");
        assert_eq!(content_type, "ssp");
        assert_eq!(cover, "https://img.example/pl1_v2.jpg");
        assert_eq!(
            pl_tracks,
            vec![(0, String::from("spotify:track:t3"))],
            "expected replaced playlist tracks, got {pl_tracks:?}"
        );

        let purged = sink
            .purge_spotify_playlists()
            .expect("purge spotify playlists");
        assert_eq!(purged, 1, "expected 1 purged playlist, got {purged}");

        let (pt_count, local_count): (i64, i64) = sink.with_connection(|conn| {
            let pt_count: i64 = conn
                .query_row("SELECT COUNT(*) FROM playlist_track", [], |r| r.get(0))
                .expect("count playlist_track");
            let local_count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM tracks WHERE url = 'file:///playlists/local.m3u'",
                    [],
                    |r| r.get(0),
                )
                .expect("count local playlist");
            (pt_count, local_count)
        });
        assert_eq!(pt_count, 0, "expected playlist_track cleared on purge");
        assert_eq!(local_count, 1, "local playlist must remain intact");
    }

    #[test]
    fn works_against_minimal_preseeded_lms_tables() {
        let conn = Connection::open_in_memory().expect("open memory db");
        conn.execute_batch(
            "CREATE TABLE tracks (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 url TEXT NOT NULL UNIQUE,
                 title TEXT,
                 tracknum INTEGER,
                 disc INTEGER,
                 secs INTEGER,
                 year INTEGER,
                 cover TEXT,
                 audio INTEGER,
                 extid TEXT,
                 content_type TEXT,
                 timestamp INTEGER
             );
             CREATE TABLE contributors (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 name TEXT,
                 namesort TEXT,
                 extid TEXT
             );
             CREATE TABLE playlist_track (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 position INTEGER,
                 playlist INTEGER,
                 track TEXT
             );",
        )
        .expect("seed minimal LMS tables");

        let mut sink = SqliteLibrarySink::new(conn).expect("wrap preseeded db");
        sink.init_online_tracks_table().expect("init online_tracks");
        let stored = sink
            .store_tracks(&[make_row("minimal1", "Minimal Track")])
            .expect("store on minimal schema");
        assert_eq!(stored, 1);

        sink.upsert_contributor("Minimal Artist", "MINIMAL ARTIST", "spotify:artist:m1")
            .expect("upsert contributor on minimal schema");
        sink.upsert_playlist(
            "spotify:playlist:m_pl",
            "Minimal List",
            "",
            &[String::from("spotify:track:minimal1")],
        )
        .expect("upsert playlist on minimal schema");
    }
}
