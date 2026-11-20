//! LMS `library.db` `SQLite` sink trait and `rusqlite` implementation.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use rusqlite::types::Value as SqlValue;
use rusqlite::{
    params, params_from_iter, Connection, OptionalExtension, Transaction, TransactionBehavior,
};
use spotty_types::{Result, SpottyError};

use crate::track_prep::LmsTrackRow;

const BUSY_TIMEOUT_SECS: u64 = 5;

const ALLOWED_TABLES: [&str; 8] = [
    "tracks",
    "contributors",
    "albums",
    "contributor_track",
    "genres",
    "genre_track",
    "playlist_track",
    "online_tracks",
];

const ALLOWED_COLUMNS: [&str; 32] = [
    "id",
    "url",
    "title",
    "titlesort",
    "titlesearch",
    "artist",
    "artist_extid",
    "album",
    "album_extid",
    "tracknum",
    "disc",
    "discc",
    "secs",
    "year",
    "cover",
    "artwork",
    "audio",
    "extid",
    "compilation",
    "release_type",
    "timestamp",
    "content_type",
    "genre",
    "account_tag",
    "name",
    "namesort",
    "namesearch",
    "role",
    "contributor",
    "track",
    "position",
    "playlist",
];

#[derive(Debug, Clone, Default)]
struct TableSchema {
    columns: HashSet<String>,
    column_types: HashMap<String, String>,
}

impl TableSchema {
    fn is_integer_col(&self, col: &str) -> bool {
        self.column_types
            .get(col)
            .is_some_and(|ty| ty.contains("INT"))
    }
}

/// Storage sink for persisting Spotify library entities into LMS `library.db`.
pub trait LibrarySink: Send + Sync {
    /// Ensures the staging `online_tracks` table exists and clears prior scan rows.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Database`] if `SQLite` table creation or cleanup
    /// fails.
    fn init_online_tracks_table(&mut self) -> Result<()>;

    /// Upserts prepared track rows and records their URIs in `online_tracks`.
    ///
    /// Returns the number of track rows processed.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Database`] if the `SQLite` transaction or statements
    /// fail.
    fn store_tracks(&mut self, rows: &[LmsTrackRow]) -> Result<usize>;

    /// Upserts a contributor record by `extid` (or `name` when `extid` is empty).
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Database`] if querying or mutating `contributors`
    /// fails.
    fn upsert_contributor(&mut self, name: &str, normalized_sort: &str, extid: &str) -> Result<()>;

    /// Upserts a batch of contributor records `(name, normalized_sort, extid)`.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Database`] if querying or mutating `contributors`
    /// fails.
    fn upsert_contributors_batch(&mut self, contributors: &[(&str, &str, &str)]) -> Result<()> {
        for (name, normalized_sort, extid) in contributors {
            self.upsert_contributor(name, normalized_sort, extid)?;
        }
        Ok(())
    }

    /// Removes all Spotify playlist rows (`url LIKE 'spotify:playlist:%'`) and
    /// their associated `playlist_track` entries.
    ///
    /// Returns the number of deleted playlist rows from `tracks`.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Database`] if the `SQLite` delete transaction fails.
    fn purge_spotify_playlists(&mut self) -> Result<usize>;

    /// Upserts a Spotify playlist with `"Spotify: {name}"` title prefix and
    /// `content_type = "ssp"`, replacing its ordered `playlist_track` rows.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Database`] if the `SQLite` upsert transaction fails.
    fn upsert_playlist(
        &mut self,
        uri: &str,
        name: &str,
        cover: &str,
        track_uris: &[String],
    ) -> Result<()>;

    /// Upserts a batch of playlists `(uri, name, cover, track_uris)`.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Database`] if the `SQLite` upsert transaction fails.
    fn upsert_playlists_batch(
        &mut self,
        playlists: &[(&str, &str, &str, &[String])],
    ) -> Result<()> {
        for (uri, name, cover, track_uris) in playlists {
            self.upsert_playlist(uri, name, cover, track_uris)?;
        }
        Ok(())
    }

    /// Deletes `spotify:track:%` rows from `tracks` that were not recorded in
    /// `online_tracks` during the current scan.
    ///
    /// Returns the number of pruned track rows.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Database`] if the `SQLite` delete transaction fails.
    fn delete_removed_tracks(&mut self) -> Result<usize>;
}

#[derive(Debug)]
struct SinkState {
    conn: Connection,
    schemas: HashMap<String, TableSchema>,
}

/// [`LibrarySink`] backed by a [`rusqlite::Connection`] to LMS `library.db`.
#[derive(Debug)]
pub struct SqliteLibrarySink {
    state: Mutex<SinkState>,
}

impl SqliteLibrarySink {
    /// Wraps an existing [`Connection`] and ensures compatible LMS tables exist.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Database`] if schema initialization fails.
    pub fn new(conn: Connection) -> Result<Self> {
        conn.busy_timeout(Duration::from_secs(BUSY_TIMEOUT_SECS))
            .map_err(db_err)?;
        ensure_schema(&conn)?;
        Ok(Self {
            state: Mutex::new(SinkState {
                conn,
                schemas: HashMap::new(),
            }),
        })
    }

    /// Opens an in-memory `SQLite` database initialized with LMS-compatible tables.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Database`] if opening or initializing `SQLite` fails.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().map_err(db_err)?;
        Self::new(conn)
    }

    /// Opens a file-backed `SQLite` database at `path` and ensures LMS tables
    /// exist.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Database`] if opening or initializing `SQLite` fails.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path).map_err(db_err)?;
        Self::new(conn)
    }

    /// Runs a read-only closure with shared access to the [`Connection`].
    ///
    /// # Panics
    ///
    /// Panics if the internal `SQLite` connection mutex was poisoned by a prior
    /// panic.
    pub fn with_connection<R>(&self, f: impl FnOnce(&Connection) -> R) -> R {
        let guard = match self.state.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        f(&guard.conn)
    }

    /// Runs a closure with exclusive mutable access to the [`Connection`].
    ///
    /// # Panics
    ///
    /// Panics if the internal `SQLite` connection mutex was poisoned by a prior
    /// panic.
    pub fn with_mut_connection<R>(&mut self, f: impl FnOnce(&mut Connection) -> R) -> R {
        let mut guard = match self.state.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.schemas.clear();
        f(&mut guard.conn)
    }

    /// Consumes this sink and returns the underlying [`Connection`].
    ///
    /// # Panics
    ///
    /// Panics if the internal `SQLite` connection mutex was poisoned by a prior
    /// panic.
    #[must_use]
    pub fn into_connection(self) -> Connection {
        let state = match self.state.into_inner() {
            Ok(s) => s,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.conn
    }

    fn with_state<R>(&mut self, f: impl FnOnce(&mut SinkState) -> Result<R>) -> Result<R> {
        let mut guard = match self.state.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        f(&mut guard)
    }
}

impl LibrarySink for SqliteLibrarySink {
    fn init_online_tracks_table(&mut self) -> Result<()> {
        self.with_state(|state| {
            let tx = state
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(db_err)?;
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

        self.with_state(|state| {
            let tx = state
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(db_err)?;
            let tracks_schema = cached_schema(&tx, &mut state.schemas, "tracks")?;
            let albums_schema = cached_schema(&tx, &mut state.schemas, "albums")?;
            let contrib_schema = cached_schema(&tx, &mut state.schemas, "contributors")?;
            let ct_schema = cached_schema(&tx, &mut state.schemas, "contributor_track")?;
            let genres_schema = cached_schema(&tx, &mut state.schemas, "genres")?;
            let gt_schema = cached_schema(&tx, &mut state.schemas, "genre_track")?;

            let album_is_fk = tracks_schema.is_integer_col("album");
            let has_albums = !albums_schema.columns.is_empty();
            let has_contrib_rel =
                !contrib_schema.columns.is_empty() && !ct_schema.columns.is_empty();
            let has_genre_rel = !genres_schema.columns.is_empty() && !gt_schema.columns.is_empty();

            for row in rows {
                let album_id = if has_albums && !row.album.is_empty() {
                    Some(upsert_album(&tx, &albums_schema, row)?)
                } else {
                    None
                };

                let fields = track_row_fields(row, album_id, album_is_fk);
                let track_id = upsert_track_fields(&tx, &tracks_schema.columns, &row.url, &fields)?;

                if has_contrib_rel && !row.artist.is_empty() {
                    link_track_contributors(&tx, &contrib_schema, &ct_schema, track_id, row)?;
                }
                if has_genre_rel && !row.genre.is_empty() {
                    link_track_genre(&tx, &genres_schema, &gt_schema, track_id, &row.genre)?;
                }

                let mut online_stmt = tx
                    .prepare_cached("INSERT OR REPLACE INTO online_tracks (url) VALUES (?1)")
                    .map_err(db_err)?;
                online_stmt.execute(params![row.url]).map_err(db_err)?;
            }

            tx.commit().map_err(db_err)?;
            Ok(rows.len())
        })
    }

    fn upsert_contributor(&mut self, name: &str, normalized_sort: &str, extid: &str) -> Result<()> {
        self.upsert_contributors_batch(&[(name, normalized_sort, extid)])
    }

    fn upsert_contributors_batch(&mut self, contributors: &[(&str, &str, &str)]) -> Result<()> {
        if contributors.is_empty() {
            return Ok(());
        }
        self.with_state(|state| {
            let tx = state
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(db_err)?;
            let cols = cached_schema(&tx, &mut state.schemas, "contributors")?;
            for &(name, normalized_sort, extid) in contributors {
                upsert_contributor_in_tx(&tx, &cols, name, normalized_sort, extid)?;
            }
            tx.commit().map_err(db_err)?;
            Ok(())
        })
    }

    fn purge_spotify_playlists(&mut self) -> Result<usize> {
        self.with_state(|state| {
            let tx = state
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(db_err)?;
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
        self.upsert_playlists_batch(&[(uri, name, cover, track_uris)])
    }

    fn upsert_playlists_batch(
        &mut self,
        playlists: &[(&str, &str, &str, &[String])],
    ) -> Result<()> {
        if playlists.is_empty() {
            return Ok(());
        }
        self.with_state(|state| {
            let tx = state
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(db_err)?;
            let tracks_schema = cached_schema(&tx, &mut state.schemas, "tracks")?;
            let pt_schema = cached_schema(&tx, &mut state.schemas, "playlist_track")?;
            let track_col_is_int = pt_schema.is_integer_col("track");

            for &(uri, name, cover, track_uris) in playlists {
                upsert_playlist_in_tx(
                    &tx,
                    &tracks_schema.columns,
                    track_col_is_int,
                    uri,
                    name,
                    cover,
                    track_uris,
                )?;
            }

            tx.commit().map_err(db_err)?;
            Ok(())
        })
    }

    fn delete_removed_tracks(&mut self) -> Result<usize> {
        self.with_state(|state| {
            let tx = state
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(db_err)?;
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

fn upsert_contributor_in_tx(
    tx: &Transaction<'_>,
    schema: &TableSchema,
    name: &str,
    normalized_sort: &str,
    extid: &str,
) -> Result<i64> {
    let existing_id: Option<i64> = if !extid.is_empty() && schema.columns.contains("extid") {
        let mut stmt = tx
            .prepare_cached("SELECT id FROM contributors WHERE extid = ?1 LIMIT 1")
            .map_err(db_err)?;
        stmt.query_row(params![extid], |r| r.get(0))
            .optional()
            .map_err(db_err)?
    } else {
        let mut stmt = tx
            .prepare_cached("SELECT id FROM contributors WHERE name = ?1 LIMIT 1")
            .map_err(db_err)?;
        stmt.query_row(params![name], |r| r.get(0))
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
        update_by_id(tx, "contributors", &schema.columns, id, &fields)?;
        Ok(id)
    } else {
        insert_row(tx, "contributors", &schema.columns, &fields)?;
        Ok(tx.last_insert_rowid())
    }
}

fn upsert_album(tx: &Transaction<'_>, schema: &TableSchema, row: &LmsTrackRow) -> Result<i64> {
    let existing_id: Option<i64> =
        if !row.album_extid.is_empty() && schema.columns.contains("extid") {
            let mut stmt = tx
                .prepare_cached("SELECT id FROM albums WHERE extid = ?1 LIMIT 1")
                .map_err(db_err)?;
            stmt.query_row(params![row.album_extid], |r| r.get(0))
                .optional()
                .map_err(db_err)?
        } else {
            let mut stmt = tx
                .prepare_cached("SELECT id FROM albums WHERE title = ?1 LIMIT 1")
                .map_err(db_err)?;
            stmt.query_row(params![row.album], |r| r.get(0))
                .optional()
                .map_err(db_err)?
        };

    let upper = row.album.to_uppercase();
    let fields = vec![
        ("title", SqlValue::Text(row.album.clone())),
        ("titlesort", SqlValue::Text(upper.clone())),
        ("titlesearch", SqlValue::Text(upper)),
        ("compilation", SqlValue::Integer(i64::from(row.compilation))),
        ("year", opt_u32(row.year)),
        ("disc", opt_u32(row.disc)),
        ("discc", opt_u32(row.discc)),
        ("extid", SqlValue::Text(row.album_extid.clone())),
        (
            "release_type",
            SqlValue::Text(row.release_type.as_lms_str().to_owned()),
        ),
        ("artwork", SqlValue::Text(row.cover.clone())),
    ];

    if let Some(id) = existing_id {
        update_by_id(tx, "albums", &schema.columns, id, &fields)?;
        Ok(id)
    } else {
        insert_row(tx, "albums", &schema.columns, &fields)?;
        Ok(tx.last_insert_rowid())
    }
}

fn link_track_contributors(
    tx: &Transaction<'_>,
    contrib_schema: &TableSchema,
    ct_schema: &TableSchema,
    track_id: i64,
    row: &LmsTrackRow,
) -> Result<()> {
    let names: Vec<&str> = row
        .artist
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let extids: Vec<&str> = row
        .artist_extid
        .as_deref()
        .map(|s| s.split(';').map(str::trim).collect())
        .unwrap_or_default();

    let pairs: Vec<(&str, &str)> = if !names.is_empty() && names.len() == extids.len() {
        names.into_iter().zip(extids).collect()
    } else {
        vec![(row.artist.trim(), row.artist_extid.as_deref().unwrap_or(""))]
    };

    for (name, extid) in pairs {
        if name.is_empty() {
            continue;
        }
        let contrib_id = upsert_contributor_in_tx(tx, contrib_schema, name, name, extid)?;
        let exists: Option<i64> = {
            let mut stmt = tx
                .prepare_cached(
                    "SELECT 1 FROM contributor_track
                     WHERE contributor = ?1 AND track = ?2 LIMIT 1",
                )
                .map_err(db_err)?;
            stmt.query_row(params![contrib_id, track_id], |r| r.get(0))
                .optional()
                .map_err(db_err)?
        };
        if exists.is_none() {
            let fields = vec![
                ("role", SqlValue::Integer(1)),
                ("contributor", SqlValue::Integer(contrib_id)),
                ("track", SqlValue::Integer(track_id)),
            ];
            insert_row(tx, "contributor_track", &ct_schema.columns, &fields)?;
        }
    }
    Ok(())
}

fn link_track_genre(
    tx: &Transaction<'_>,
    genres_schema: &TableSchema,
    gt_schema: &TableSchema,
    track_id: i64,
    genre_name: &str,
) -> Result<()> {
    let existing_id: Option<i64> = {
        let mut stmt = tx
            .prepare_cached("SELECT id FROM genres WHERE name = ?1 LIMIT 1")
            .map_err(db_err)?;
        stmt.query_row(params![genre_name], |r| r.get(0))
            .optional()
            .map_err(db_err)?
    };
    let genre_id = if let Some(id) = existing_id {
        id
    } else {
        let upper = genre_name.to_uppercase();
        let fields = vec![
            ("name", SqlValue::Text(genre_name.to_owned())),
            ("namesort", SqlValue::Text(upper.clone())),
            ("namesearch", SqlValue::Text(upper)),
        ];
        insert_row(tx, "genres", &genres_schema.columns, &fields)?;
        tx.last_insert_rowid()
    };

    let exists: Option<i64> = {
        let mut stmt = tx
            .prepare_cached("SELECT 1 FROM genre_track WHERE genre = ?1 AND track = ?2 LIMIT 1")
            .map_err(db_err)?;
        stmt.query_row(params![genre_id, track_id], |r| r.get(0))
            .optional()
            .map_err(db_err)?
    };
    if exists.is_none() {
        let fields = vec![
            ("genre", SqlValue::Integer(genre_id)),
            ("track", SqlValue::Integer(track_id)),
        ];
        insert_row(tx, "genre_track", &gt_schema.columns, &fields)?;
    }
    Ok(())
}

fn upsert_playlist_in_tx(
    tx: &Transaction<'_>,
    tracks_cols: &HashSet<String>,
    track_col_is_int: bool,
    uri: &str,
    name: &str,
    cover: &str,
    track_uris: &[String],
) -> Result<()> {
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

    let playlist_id = upsert_track_fields(tx, tracks_cols, uri, &fields)?;

    {
        let mut del_stmt = tx
            .prepare_cached("DELETE FROM playlist_track WHERE playlist = ?1")
            .map_err(db_err)?;
        del_stmt.execute(params![playlist_id]).map_err(db_err)?;
    }

    for (idx, track_uri) in track_uris.iter().enumerate() {
        let pos = i64::try_from(idx)
            .map_err(|err| SpottyError::Database(format!("playlist index overflow: {err}")))?;
        let track_val = if track_col_is_int {
            let track_id = resolve_or_stub_track_id(tx, tracks_cols, track_uri)?;
            SqlValue::Integer(track_id)
        } else {
            SqlValue::Text(track_uri.clone())
        };
        let mut ins_stmt = tx
            .prepare_cached(
                "INSERT INTO playlist_track (position, playlist, track) VALUES (?1, ?2, ?3)",
            )
            .map_err(db_err)?;
        ins_stmt
            .execute(params![pos, playlist_id, track_val])
            .map_err(db_err)?;
    }

    Ok(())
}

fn resolve_or_stub_track_id(
    tx: &Transaction<'_>,
    tracks_cols: &HashSet<String>,
    track_uri: &str,
) -> Result<i64> {
    let existing: Option<i64> = {
        let mut stmt = tx
            .prepare_cached("SELECT id FROM tracks WHERE url = ?1 LIMIT 1")
            .map_err(db_err)?;
        stmt.query_row(params![track_uri], |r| r.get(0))
            .optional()
            .map_err(db_err)?
    };
    if let Some(id) = existing {
        return Ok(id);
    }

    let upper = track_uri.to_uppercase();
    let fields = vec![
        ("url", SqlValue::Text(track_uri.to_owned())),
        ("title", SqlValue::Text(track_uri.to_owned())),
        ("titlesort", SqlValue::Text(upper.clone())),
        ("titlesearch", SqlValue::Text(upper)),
        ("audio", SqlValue::Integer(1)),
        ("extid", SqlValue::Text(track_uri.to_owned())),
        ("content_type", SqlValue::Text(String::from("spt"))),
    ];
    insert_row(tx, "tracks", tracks_cols, &fields)?;
    Ok(tx.last_insert_rowid())
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

fn track_row_fields(
    row: &LmsTrackRow,
    album_id: Option<i64>,
    album_is_fk: bool,
) -> Vec<(&'static str, SqlValue)> {
    let album_val = match (album_is_fk, album_id) {
        (true, Some(id)) => SqlValue::Integer(id),
        (true, None) => SqlValue::Null,
        (false, _) => SqlValue::Text(row.album.clone()),
    };
    vec![
        ("url", SqlValue::Text(row.url.clone())),
        ("title", SqlValue::Text(row.title.clone())),
        ("titlesort", SqlValue::Text(row.title.to_uppercase())),
        ("titlesearch", SqlValue::Text(row.title.to_uppercase())),
        ("artist", SqlValue::Text(row.artist.clone())),
        ("artist_extid", opt_text(row.artist_extid.as_deref())),
        ("album", album_val),
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

fn validate_table_name(table: &str) -> Result<&'static str> {
    ALLOWED_TABLES
        .iter()
        .copied()
        .find(|&allowed| allowed == table)
        .ok_or_else(|| SpottyError::Database(format!("disallowed SQLite table: {table}")))
}

fn validate_column_name(col: &str) -> Result<()> {
    if ALLOWED_COLUMNS.contains(&col) {
        Ok(())
    } else {
        Err(SpottyError::Database(format!(
            "disallowed SQLite column: {col}"
        )))
    }
}

fn cached_schema(
    tx: &Transaction<'_>,
    cache: &mut HashMap<String, TableSchema>,
    table: &str,
) -> Result<TableSchema> {
    let safe_table = validate_table_name(table)?;
    if let Some(existing) = cache.get(safe_table) {
        return Ok(existing.clone());
    }
    let pragma = format!("PRAGMA table_info({safe_table})");
    let mut stmt = tx.prepare(&pragma).map_err(db_err)?;
    let mut rows = stmt.query([]).map_err(db_err)?;
    let mut schema = TableSchema::default();
    while let Some(row) = rows.next().map_err(db_err)? {
        let name: String = row.get(1).map_err(db_err)?;
        let ty: String = row.get(2).map_err(db_err)?;
        schema.column_types.insert(name.clone(), ty.to_uppercase());
        schema.columns.insert(name);
    }
    cache.insert(safe_table.to_owned(), schema.clone());
    Ok(schema)
}

fn upsert_track_fields(
    tx: &Transaction<'_>,
    cols: &HashSet<String>,
    url: &str,
    fields: &[(&'static str, SqlValue)],
) -> Result<i64> {
    let existing_id: Option<i64> = {
        let mut stmt = tx
            .prepare_cached("SELECT id FROM tracks WHERE url = ?1 LIMIT 1")
            .map_err(db_err)?;
        stmt.query_row(params![url], |r| r.get(0))
            .optional()
            .map_err(db_err)?
    };

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
    let safe_table = validate_table_name(table)?;
    let mut assignments = Vec::new();
    let mut values = Vec::new();
    for (col, val) in fields {
        if *col == "url" || !cols.contains(*col) {
            continue;
        }
        validate_column_name(col)?;
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
        "UPDATE {safe_table} SET {} WHERE id = ?{id_idx}",
        assignments.join(", ")
    );
    let mut stmt = tx.prepare_cached(&sql).map_err(db_err)?;
    stmt.execute(params_from_iter(values)).map_err(db_err)?;
    Ok(())
}

fn insert_row(
    tx: &Transaction<'_>,
    table: &str,
    cols: &HashSet<String>,
    fields: &[(&'static str, SqlValue)],
) -> Result<()> {
    let safe_table = validate_table_name(table)?;
    let mut col_names = Vec::new();
    let mut placeholders = Vec::new();
    let mut values = Vec::new();
    for (col, val) in fields {
        if !cols.contains(*col) {
            continue;
        }
        validate_column_name(col)?;
        let idx = values.len() + 1;
        col_names.push(*col);
        placeholders.push(format!("?{idx}"));
        values.push(val.clone());
    }
    if col_names.is_empty() {
        return Ok(());
    }
    let sql = format!(
        "INSERT INTO {safe_table} ({}) VALUES ({})",
        col_names.join(", "),
        placeholders.join(", ")
    );
    let mut stmt = tx.prepare_cached(&sql).map_err(db_err)?;
    stmt.execute(params_from_iter(values)).map_err(db_err)?;
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

    #[test]
    fn configures_five_second_busy_timeout_and_rejects_disallowed_identifiers() {
        let sink = SqliteLibrarySink::open_in_memory().expect("open sink");
        let timeout_ms: i64 = sink.with_connection(|conn| {
            conn.query_row("PRAGMA busy_timeout", [], |r| r.get(0))
                .expect("query busy_timeout")
        });
        assert_eq!(timeout_ms, 5_000, "busy_timeout must be 5000ms");

        assert!(validate_table_name("tracks").is_ok());
        assert!(validate_table_name("tracks; DROP TABLE tracks").is_err());
        assert!(validate_column_name("title").is_ok());
        assert!(validate_column_name("1=1; --").is_err());
    }

    #[test]
    fn populates_real_lms_relational_tables_and_integer_playlist_fk() {
        let conn = Connection::open_in_memory().expect("open memory db");
        conn.execute_batch(
            "CREATE TABLE tracks (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 url TEXT NOT NULL UNIQUE,
                 title TEXT,
                 titlesort TEXT,
                 titlesearch TEXT,
                 album INTEGER,
                 tracknum INTEGER,
                 disc INTEGER,
                 secs INTEGER,
                 year INTEGER,
                 cover TEXT,
                 audio INTEGER,
                 extid TEXT,
                 timestamp INTEGER,
                 content_type TEXT
             );
             CREATE TABLE albums (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 title TEXT,
                 titlesort TEXT,
                 titlesearch TEXT,
                 compilation INTEGER,
                 year INTEGER,
                 disc INTEGER,
                 discc INTEGER,
                 extid TEXT,
                 release_type TEXT,
                 artwork TEXT
             );
             CREATE TABLE contributors (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 name TEXT,
                 namesort TEXT,
                 namesearch TEXT,
                 extid TEXT
             );
             CREATE TABLE contributor_track (
                 role INTEGER,
                 contributor INTEGER,
                 track INTEGER
             );
             CREATE TABLE genres (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 name TEXT,
                 namesort TEXT,
                 namesearch TEXT
             );
             CREATE TABLE genre_track (
                 genre INTEGER,
                 track INTEGER
             );
             CREATE TABLE playlist_track (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 position INTEGER NOT NULL,
                 playlist INTEGER NOT NULL,
                 track INTEGER NOT NULL
             );",
        )
        .expect("create real LMS relational schema");

        let mut sink = SqliteLibrarySink::new(conn).expect("wrap real LMS schema");
        sink.init_online_tracks_table().expect("init online_tracks");
        sink.store_tracks(&[make_row("rel1", "Relational Song")])
            .expect("store track into relational schema");
        sink.upsert_playlist(
            "spotify:playlist:rel_pl",
            "Rel Mix",
            "",
            &[
                String::from("spotify:track:rel1"),
                String::from("spotify:track:rel_stub"),
            ],
        )
        .expect("upsert playlist with integer FK");

        sink.with_connection(|conn| {
            let track_id: i64 = conn
                .query_row(
                    "SELECT id FROM tracks WHERE url = 'spotify:track:rel1'",
                    [],
                    |r| r.get(0),
                )
                .expect("query track id");
            let album_fk: i64 = conn
                .query_row(
                    "SELECT album FROM tracks WHERE id = ?1",
                    params![track_id],
                    |r| r.get(0),
                )
                .expect("query tracks.album FK");
            let album_title: String = conn
                .query_row(
                    "SELECT title FROM albums WHERE id = ?1",
                    params![album_fk],
                    |r| r.get(0),
                )
                .expect("join albums");
            assert_eq!(album_title, "Album 1");

            let contrib_name: String = conn
                .query_row(
                    "SELECT c.name FROM contributors c
                     JOIN contributor_track ct ON ct.contributor = c.id
                     WHERE ct.track = ?1",
                    params![track_id],
                    |r| r.get(0),
                )
                .expect("join contributor_track");
            assert_eq!(contrib_name, "Artist 1");

            let genre_name: String = conn
                .query_row(
                    "SELECT g.name FROM genres g
                     JOIN genre_track gt ON gt.genre = g.id
                     WHERE gt.track = ?1",
                    params![track_id],
                    |r| r.get(0),
                )
                .expect("join genre_track");
            assert_eq!(genre_name, "Spotify");

            let pt_track_fk: i64 = conn
                .query_row(
                    "SELECT track FROM playlist_track WHERE position = 0",
                    [],
                    |r| r.get(0),
                )
                .expect("query integer playlist_track.track");
            assert_eq!(pt_track_fk, track_id);

            let stub_url: String = conn
                .query_row(
                    "SELECT t.url FROM tracks t
                     JOIN playlist_track pt ON pt.track = t.id
                     WHERE pt.position = 1",
                    [],
                    |r| r.get(0),
                )
                .expect("join stubbed playlist track");
            assert_eq!(stub_url, "spotify:track:rel_stub");
        });
    }
}
