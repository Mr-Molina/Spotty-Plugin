//! Bounded 50-item case-insensitive recent searches store and LMS CLI handler.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use spotty_cache::DualCache;
use spotty_cache::MetadataWriteMode;
use spotty_types::Result;
use spotty_types::SpottyError;

/// Maximum number of retained recent search queries (`OPML.pm:MAX_RECENT`).
pub const MAX_RECENT_SEARCHES: usize = 50;

/// Persistent `DualCache` metadata key storing recent search queries.
pub const RECENT_SEARCHES_CACHE_KEY: &str = "spotty_recent_searches";

/// Parsed query actions for the `spotty recentsearches` LMS CLI command.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecentSearchCliQuery {
    /// Returns either the delete-confirmation menu for `delete_menu_idx` or
    /// the current MRU history entries when `delete_menu_idx` is `None`.
    ListMenu {
        /// Optional zero-based index into the stored oldest-to-newest list.
        delete_menu_idx: Option<usize>,
    },
    /// Returns the delete-confirmation menu keyed by exact search term.
    DeleteMenuTerm(String),
    /// Deletes the search entry at the given stored index.
    DeleteIndex(usize),
    /// Deletes the search entry matching the given term.
    DeleteTerm(String),
    /// Clears all stored search history entries.
    DeleteAll,
}

/// Single LMS CLI `item_loop` entry returned by `spotty recentsearches`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentSearchCliItem {
    /// Localized display text for the menu item.
    pub text: String,
    /// Target LMS CLI command tokens (e.g. `["spotty", "recentsearches"]`).
    pub cmd: Vec<String>,
    /// Parameters dispatched when selecting this action.
    pub params: BTreeMap<String, String>,
    /// Target LMS UI window navigation (`"parent"` or `"grandParent"`).
    pub next_window: String,
}

/// Structured LMS CLI response matching `_recentSearchesCLI` in `OPML.pm`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RecentSearchCliResponse {
    /// Response slice offset (`0`).
    pub offset: usize,
    /// Number of items in `item_loop`.
    pub count: usize,
    /// Menu items returned to the LMS CLI client.
    pub item_loop: Vec<RecentSearchCliItem>,
}

/// Bounded 50-item case-insensitive deduplicated MRU/LRU search history store.
///
/// Internally stores entries from oldest (`index 0`) to newest (`len - 1`),
/// matching `spotify_recent_search` in `OPML.pm`.
#[non_exhaustive]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RecentSearchesStore {
    items: Vec<String>,
    #[serde(skip)]
    cache: Option<Arc<DualCache>>,
}

impl PartialEq for RecentSearchesStore {
    fn eq(&self, other: &Self) -> bool {
        self.items == other.items
    }
}

impl Eq for RecentSearchesStore {}

impl RecentSearchesStore {
    /// Creates an empty [`RecentSearchesStore`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Loads persisted recent searches from `cache`, returning an empty store
    /// backed by `cache` if no entry exists yet.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Cache`] or [`SpottyError::Json`] if reading or
    /// decoding the persisted entry fails.
    pub fn open_or_load(cache: Arc<DualCache>) -> Result<Self> {
        let loaded = match cache.metadata_value(RECENT_SEARCHES_CACHE_KEY)? {
            Some(val) => {
                let raw: Vec<String> = serde_json::from_value(val)?;
                Self::from_items(raw)
            }
            None => Self::new(),
        };
        Ok(loaded.with_cache(cache))
    }

    /// Attaches a [`DualCache`] backing store for persistence across restarts.
    #[must_use]
    pub fn with_cache(mut self, cache: Arc<DualCache>) -> Self {
        self.cache = Some(cache);
        self
    }

    /// Persists the current history list to the attached [`DualCache`], if
    /// any.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Cache`] or [`SpottyError::Json`] on storage
    /// failure.
    pub fn save(&self) -> Result<()> {
        let Some(cache) = self.cache.as_ref() else {
            return Ok(());
        };
        let value = Value::Array(
            self.items
                .iter()
                .map(|item| Value::String(item.clone()))
                .collect(),
        );
        cache.store_metadata(
            RECENT_SEARCHES_CACHE_KEY,
            None,
            &value,
            MetadataWriteMode::OverwriteDirect,
        )
    }

    /// Clears all stored search history entries and persists the empty
    /// state.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Cache`] or [`SpottyError::Json`] if persistence
    /// fails.
    pub fn clear(&mut self) -> Result<()> {
        self.items.clear();
        self.save()
    }

    /// Reconstructs a [`RecentSearchesStore`] from persisted oldest-to-newest
    /// entries, deduplicating case-insensitively and capping to
    /// [`MAX_RECENT_SEARCHES`].
    #[must_use]
    pub fn from_items(raw_items: Vec<String>) -> Self {
        let mut store = Self::new();
        for entry in raw_items {
            store.add(&entry);
        }
        store
    }

    /// Returns `true` when no recent searches are stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Returns the number of stored recent search terms.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Returns stored entries from oldest to newest (`spotify_recent_search`).
    #[must_use]
    pub fn items(&self) -> &[String] {
        &self.items
    }

    /// Returns stored entries ordered newest-first for UI display.
    #[must_use]
    pub fn mru_items(&self) -> Vec<&str> {
        let mut out = Vec::with_capacity(self.items.len());
        for item in self.items.iter().rev() {
            out.push(item.as_str());
        }
        out
    }

    /// Records a search query, deduplicating case-insensitively and evicting
    /// the oldest entry when exceeding [`MAX_RECENT_SEARCHES`].
    pub fn add(&mut self, query: &str) {
        self.add_in_memory(query);
        let _ = self.save();
    }

    /// Records a search query and returns any persistence error from the
    /// attached [`DualCache`].
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Cache`] or [`SpottyError::Json`] if persistence
    /// fails.
    pub fn add_persisted(&mut self, query: &str) -> Result<()> {
        self.add_in_memory(query);
        self.save()
    }

    fn add_in_memory(&mut self, query: &str) {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return;
        }

        let needle_lower = (!trimmed.is_ascii()).then(|| trimmed.to_lowercase());
        self.items.retain(|existing| {
            if let Some(lower) = needle_lower.as_deref() {
                existing.to_lowercase() != lower
            } else {
                !existing.eq_ignore_ascii_case(trimmed)
            }
        });
        self.items.push(String::from(trimmed));

        if self.items.len() > MAX_RECENT_SEARCHES {
            let overflow = self.items.len() - MAX_RECENT_SEARCHES;
            self.items.drain(0..overflow);
        }
    }

    /// Deletes a stored search term by value (case-insensitively) and persists
    /// the updated history if a [`DualCache`] is attached.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Cache`] if `term` is not present or persistence
    /// fails.
    pub fn delete_term(&mut self, term: &str) -> Result<()> {
        let trimmed = term.trim();
        let needle_lower = (!trimmed.is_ascii()).then(|| trimmed.to_lowercase());
        let before = self.items.len();
        self.items.retain(|existing| {
            if let Some(lower) = needle_lower.as_deref() {
                existing.to_lowercase() != lower
            } else {
                !existing.eq_ignore_ascii_case(trimmed)
            }
        });
        if self.items.len() == before {
            return Err(SpottyError::Cache(format!(
                "search term '{trimmed}' not found in history"
            )));
        }
        self.save()
    }

    /// Executes a `spotty recentsearches` CLI query (`OPML.pm:1837-1898`).
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Cache`] when `delete_menu_idx` or `DeleteIndex`
    /// references an out-of-range index or an empty history list.
    pub fn handle_cli_query(
        &mut self,
        query: RecentSearchCliQuery,
        delete_label: &str,
        clear_label: &str,
    ) -> Result<RecentSearchCliResponse> {
        match query {
            RecentSearchCliQuery::ListMenu {
                delete_menu_idx: Some(idx),
            } => {
                let Some(target) = self.items.get(idx) else {
                    return Err(SpottyError::Cache(format!(
                        "search item index {idx} is outside history list (len {})",
                        self.items.len()
                    )));
                };

                let mut delete_params = BTreeMap::new();
                delete_params.insert(String::from("delete"), idx.to_string());

                let mut clear_params = BTreeMap::new();
                clear_params.insert(String::from("deleteAll"), String::from("1"));

                let item_loop = vec![
                    RecentSearchCliItem {
                        text: format!("{delete_label}: \"{target}\""),
                        cmd: vec![String::from("spotty"), String::from("recentsearches")],
                        params: delete_params,
                        next_window: String::from("parent"),
                    },
                    RecentSearchCliItem {
                        text: String::from(clear_label),
                        cmd: vec![String::from("spotty"), String::from("recentsearches")],
                        params: clear_params,
                        next_window: String::from("grandParent"),
                    },
                ];

                Ok(RecentSearchCliResponse {
                    offset: 0,
                    count: item_loop.len(),
                    item_loop,
                })
            }
            RecentSearchCliQuery::DeleteMenuTerm(term) => {
                let target = self
                    .items
                    .iter()
                    .find(|existing| existing.eq_ignore_ascii_case(term.trim()))
                    .ok_or_else(|| {
                        SpottyError::Cache(format!(
                            "search term '{}' not found in history",
                            term.trim()
                        ))
                    })?;

                let mut delete_params = BTreeMap::new();
                delete_params.insert(String::from("deleteTerm"), target.clone());

                let mut clear_params = BTreeMap::new();
                clear_params.insert(String::from("deleteAll"), String::from("1"));

                let item_loop = vec![
                    RecentSearchCliItem {
                        text: format!("{delete_label}: \"{target}\""),
                        cmd: vec![String::from("spotty"), String::from("recentsearches")],
                        params: delete_params,
                        next_window: String::from("parent"),
                    },
                    RecentSearchCliItem {
                        text: String::from(clear_label),
                        cmd: vec![String::from("spotty"), String::from("recentsearches")],
                        params: clear_params,
                        next_window: String::from("grandParent"),
                    },
                ];

                Ok(RecentSearchCliResponse {
                    offset: 0,
                    count: item_loop.len(),
                    item_loop,
                })
            }
            RecentSearchCliQuery::ListMenu {
                delete_menu_idx: None,
            } => {
                let mut item_loop = Vec::with_capacity(self.items.len());
                for (idx, term) in self.items.iter().enumerate().rev() {
                    let mut params = BTreeMap::new();
                    params.insert(String::from("deleteMenu"), idx.to_string());
                    params.insert(String::from("deleteTerm"), term.clone());
                    item_loop.push(RecentSearchCliItem {
                        text: term.clone(),
                        cmd: vec![String::from("spotty"), String::from("recentsearches")],
                        params,
                        next_window: String::new(),
                    });
                }

                Ok(RecentSearchCliResponse {
                    offset: 0,
                    count: item_loop.len(),
                    item_loop,
                })
            }
            RecentSearchCliQuery::DeleteAll => {
                self.clear()?;
                Ok(RecentSearchCliResponse::default())
            }
            RecentSearchCliQuery::DeleteIndex(idx) => {
                if idx >= self.items.len() {
                    return Err(SpottyError::Cache(format!(
                        "search item index {idx} is outside history list (len {})",
                        self.items.len()
                    )));
                }
                self.items.remove(idx);
                self.save()?;
                Ok(RecentSearchCliResponse::default())
            }
            RecentSearchCliQuery::DeleteTerm(term) => {
                self.delete_term(&term)?;
                Ok(RecentSearchCliResponse::default())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_deduplicates_case_insensitively_and_caps_at_50() {
        let mut store = RecentSearchesStore::new();
        assert!(store.is_empty());

        for idx in 1..=55 {
            store.add(&format!("Query {idx}"));
        }

        assert_eq!(store.len(), MAX_RECENT_SEARCHES);
        assert_eq!(store.items().first().map(String::as_str), Some("Query 6"));
        assert_eq!(store.items().last().map(String::as_str), Some("Query 55"));

        // Case-insensitive duplicate moves to MRU tail with updated casing.
        store.add("  query 10 ");
        assert_eq!(store.len(), MAX_RECENT_SEARCHES);
        assert_eq!(store.items().last().map(String::as_str), Some("query 10"));
        assert_eq!(store.mru_items().first().copied(), Some("query 10"));
        assert_eq!(
            store
                .items()
                .iter()
                .filter(|item| item.eq_ignore_ascii_case("query 10"))
                .count(),
            1,
            "duplicate query should appear exactly once"
        );
    }

    #[test]
    fn cli_query_handles_menu_delete_index_delete_all_and_bounds() {
        let mut store = RecentSearchesStore::new();
        store.add("Miles Davis");
        store.add("John Coltrane");

        let menu = store
            .handle_cli_query(
                RecentSearchCliQuery::ListMenu {
                    delete_menu_idx: Some(1),
                },
                "Delete",
                "Clear search history",
            )
            .expect("valid deleteMenu index should succeed");
        assert_eq!(menu.offset, 0);
        assert_eq!(menu.count, 2);
        assert_eq!(menu.item_loop[0].text, "Delete: \"John Coltrane\"");
        assert_eq!(
            menu.item_loop[0].params.get("delete").map(String::as_str),
            Some("1")
        );
        assert_eq!(menu.item_loop[0].next_window, "parent");
        assert_eq!(menu.item_loop[1].text, "Clear search history");
        assert_eq!(
            menu.item_loop[1]
                .params
                .get("deleteAll")
                .map(String::as_str),
            Some("1")
        );
        assert_eq!(menu.item_loop[1].next_window, "grandParent");

        let list = store
            .handle_cli_query(
                RecentSearchCliQuery::ListMenu {
                    delete_menu_idx: None,
                },
                "Delete",
                "Clear search history",
            )
            .expect("listing MRU menu should succeed");
        assert_eq!(list.count, 2);
        assert_eq!(list.item_loop[0].text, "John Coltrane");
        assert_eq!(list.item_loop[1].text, "Miles Davis");

        store
            .handle_cli_query(
                RecentSearchCliQuery::DeleteIndex(0),
                "Delete",
                "Clear search history",
            )
            .expect("delete index 0 should succeed");
        assert_eq!(store.items(), &[String::from("John Coltrane")]);

        let oob = store.handle_cli_query(
            RecentSearchCliQuery::DeleteIndex(5),
            "Delete",
            "Clear search history",
        );
        assert!(oob.is_err(), "out-of-bounds delete should return error");

        store
            .handle_cli_query(
                RecentSearchCliQuery::DeleteAll,
                "Delete",
                "Clear search history",
            )
            .expect("deleteAll should clear history");
        assert!(store.is_empty());

        let empty_menu = store.handle_cli_query(
            RecentSearchCliQuery::ListMenu {
                delete_menu_idx: Some(0),
            },
            "Delete",
            "Clear search history",
        );
        assert!(empty_menu.is_err(), "deleteMenu on empty list must fail");
    }

    #[test]
    fn dual_cache_persists_and_term_deletion_avoids_index_shift() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let cache = Arc::new(DualCache::open(tmp.path().join("cache.redb")).expect("cache"));

        let mut store = RecentSearchesStore::open_or_load(Arc::clone(&cache)).expect("open");
        store.add_persisted("Miles Davis").expect("add 1");
        store.add_persisted("John Coltrane").expect("add 2");

        let mut reloaded = RecentSearchesStore::open_or_load(Arc::clone(&cache)).expect("reload");
        assert_eq!(reloaded.items(), &["Miles Davis", "John Coltrane"]);

        // Simulate concurrent insert shifting indices before deleting
        // "Miles Davis" by term.
        reloaded.add_persisted("Bill Evans").expect("add 3");
        let menu = reloaded
            .handle_cli_query(
                RecentSearchCliQuery::DeleteMenuTerm(String::from("Miles Davis")),
                "Delete",
                "Clear",
            )
            .expect("deleteMenuTerm");
        assert_eq!(
            menu.item_loop[0]
                .params
                .get("deleteTerm")
                .map(String::as_str),
            Some("Miles Davis")
        );

        reloaded
            .handle_cli_query(
                RecentSearchCliQuery::DeleteTerm(String::from("Miles Davis")),
                "Delete",
                "Clear",
            )
            .expect("deleteTerm");

        let after_delete =
            RecentSearchesStore::open_or_load(Arc::clone(&cache)).expect("reload after delete");
        assert_eq!(after_delete.items(), &["John Coltrane", "Bill Evans"]);

        let mut cleared = after_delete;
        cleared.clear().expect("clear");
        let after_clear =
            RecentSearchesStore::open_or_load(Arc::clone(&cache)).expect("reload after clear");
        assert!(after_clear.is_empty());
    }
}
