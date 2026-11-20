//! Deterministic JSON deep-merge, tag cleanup, and artwork selection helpers.
//!
//! Replaces the legacy Perl `Hash::Merge` `NO_DUPLICATES` behavior and
//! `API::Cache` tag/artwork normalization utilities.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

const TAG_KEYWORDS: [&str; 5] = ["deluxe", "edition", "remaster", "live", "anniversary"];

/// Artwork image descriptor returned by Spotify Web API metadata payloads.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtworkImage {
    /// Absolute HTTPS image URL.
    pub url: String,
    /// Pixel height reported by Spotify, if known.
    pub height: Option<u32>,
    /// Pixel width reported by Spotify, if known.
    pub width: Option<u32>,
}

impl ArtworkImage {
    /// Creates a new [`ArtworkImage`] descriptor.
    #[must_use]
    pub fn new(url: impl Into<String>, height: Option<u32>, width: Option<u32>) -> Self {
        Self {
            url: url.into(),
            height,
            width,
        }
    }
}

/// Deep-merges `incoming` into `existing` with `NO_DUPLICATES` semantics.
///
/// - Objects merge recursively, keeping `existing` keys such as `image` or
///   `added_at` when omitted from `incoming`.
/// - Arrays concatenate `existing` followed by `incoming`, deduplicating
///   object elements that carry a non-empty string `"uri"` field while
///   preserving first-seen order.
/// - Scalars and mismatched value kinds use right-precedence (`incoming`).
#[must_use]
pub fn merge_no_duplicates(existing: &Value, incoming: &Value) -> Value {
    match (existing, incoming) {
        (Value::Object(left), Value::Object(right)) => {
            let mut merged: Map<String, Value> = left.clone();
            for (key, right_val) in right {
                if let Some(left_val) = merged.get(key) {
                    let combined = merge_no_duplicates(left_val, right_val);
                    merged.insert(key.clone(), combined);
                } else {
                    merged.insert(key.clone(), right_val.clone());
                }
            }
            Value::Object(merged)
        }
        (Value::Array(left), Value::Array(right)) => {
            let mut out = Vec::with_capacity(left.len().saturating_add(right.len()));
            let mut seen_uris = HashSet::new();
            push_deduped_items(left, &mut out, &mut seen_uris);
            push_deduped_items(right, &mut out, &mut seen_uris);
            Value::Array(out)
        }
        _ => incoming.clone(),
    }
}

fn push_deduped_items(items: &[Value], out: &mut Vec<Value>, seen_uris: &mut HashSet<String>) {
    for item in items {
        let maybe_uri = item
            .as_object()
            .and_then(|obj| obj.get("uri"))
            .and_then(Value::as_str)
            .filter(|uri| !uri.is_empty());

        if let Some(uri) = maybe_uri {
            if seen_uris.insert(uri.to_owned()) {
                out.push(item.clone());
            }
        } else {
            out.push(item.clone());
        }
    }
}

/// Strips Spotify edition/remaster/live/anniversary tags (`API/Cache.pm`).
///
/// Removes bracketed `(...)` / `[...]` segments and trailing `" -..."` suffixes
/// whose segment contains `deluxe`, `edition`, `remaster`, `live`, or
/// `anniversary` (case-insensitive), then trims surrounding whitespace.
#[must_use]
pub fn cleanup_tags(text: &str) -> String {
    let without_brackets = strip_keyword_brackets(text);
    let without_dash_suffix = strip_keyword_dash_suffix(&without_brackets);
    without_dash_suffix.trim().to_owned()
}

fn contains_tag_keyword(segment: &str) -> bool {
    let lower = segment.to_ascii_lowercase();
    for keyword in TAG_KEYWORDS {
        if lower.contains(keyword) {
            return true;
        }
    }
    false
}

fn strip_keyword_brackets(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut idx = 0;
    let bytes = text.as_bytes();

    while idx < text.len() {
        let ch_byte = bytes[idx];
        if ch_byte == b'(' || ch_byte == b'[' {
            if let Some(rel_end) = text[idx + 1..].find([')', ']']) {
                let close_idx = idx + 1 + rel_end;
                let inner = &text[idx + 1..close_idx];
                if contains_tag_keyword(inner) {
                    idx = close_idx + 1;
                    continue;
                }
            }
        }
        let Some(ch) = text[idx..].chars().next() else {
            break;
        };
        out.push(ch);
        idx += ch.len_utf8();
    }

    out
}

fn strip_keyword_dash_suffix(text: &str) -> &str {
    let mut search_from = 0;
    while let Some(rel_pos) = text[search_from..].find(" -") {
        let dash_pos = search_from + rel_pos;
        let after_dash = &text[dash_pos + 2..];
        let segment_end = after_dash.find(['-', '\n']).unwrap_or(after_dash.len());
        let segment = &after_dash[..segment_end];
        if contains_tag_keyword(segment) {
            return &text[..dash_pos];
        }
        search_from = dash_pos + 2;
    }
    text
}

/// Returns the URL of the tallest artwork image, or `""` when empty.
///
/// Mirrors `Plugins::Spotty::API::Cache::getLargestArtwork`, breaking height
/// ties by width and then preserving the first matching entry.
#[must_use]
pub fn largest_artwork_url(images: &[ArtworkImage]) -> &str {
    let mut best: Option<&ArtworkImage> = None;
    let mut best_dims = (0_u32, 0_u32);

    for image in images {
        let dims = (image.height.unwrap_or(0), image.width.unwrap_or(0));
        if best.is_none() || dims > best_dims {
            best = Some(image);
            best_dims = dims;
        }
    }

    best.map_or("", |img| img.url.as_str())
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use proptest::collection::vec;
    use proptest::prelude::{any, prop_assert, prop_assert_eq, proptest};
    use serde_json::{json, Value};

    use super::{cleanup_tags, largest_artwork_url, merge_no_duplicates, ArtworkImage};

    #[test]
    fn merge_preserves_existing_album_image_and_added_at() {
        let existing = json!({
            "uri": "spotify:track:1",
            "name": "Track One",
            "added_at": "2025-01-15T10:00:00Z",
            "album": {
                "uri": "spotify:album:9",
                "name": "Album Nine",
                "image": "https://i.scdn.co/image/large"
            }
        });
        let incoming = json!({
            "uri": "spotify:track:1",
            "name": "Track One (Updated)",
            "album": {
                "uri": "spotify:album:9",
                "name": "Album Nine"
            }
        });

        let merged = merge_no_duplicates(&existing, &incoming);
        assert_eq!(merged["name"], "Track One (Updated)");
        assert_eq!(merged["added_at"], "2025-01-15T10:00:00Z");
        assert_eq!(merged["album"]["image"], "https://i.scdn.co/image/large");
    }

    #[test]
    fn merge_deduplicates_object_arrays_by_uri_preserving_order() {
        let existing = json!({
            "tracks": [
                {"uri": "spotify:track:a", "name": "Alpha"},
                {"uri": "spotify:track:b", "name": "Beta"},
                {"name": "NoUriFirst"}
            ]
        });
        let incoming = json!({
            "tracks": [
                {"uri": "spotify:track:b", "name": "Beta Dup"},
                {"uri": "spotify:track:c", "name": "Gamma"},
                {"name": "NoUriSecond"}
            ]
        });

        let merged = merge_no_duplicates(&existing, &incoming);
        let tracks = merged["tracks"]
            .as_array()
            .expect("merged tracks must be a JSON array");
        assert_eq!(
            tracks.len(),
            5,
            "expected 5 tracks after URI dedup: {tracks:?}"
        );
        assert_eq!(tracks[0]["uri"], "spotify:track:a");
        assert_eq!(tracks[1]["name"], "Beta");
        assert_eq!(tracks[2]["name"], "NoUriFirst");
        assert_eq!(tracks[3]["uri"], "spotify:track:c");
        assert_eq!(tracks[4]["name"], "NoUriSecond");
    }

    #[test]
    fn cleanup_tags_strips_editions_remasters_and_live_suffixes() {
        let cases = [
            ("Abbey Road (2011 Remaster)", "Abbey Road"),
            ("Nevermind [Deluxe Edition]", "Nevermind"),
            ("Thriller (25th Anniversary Edition)", "Thriller"),
            ("Wembley (Live)", "Wembley"),
            (
                "Dark Side of the Moon - 2011 Remaster",
                "Dark Side of the Moon",
            ),
            (
                "Hotel California - Live at The Forum, 1976",
                "Hotel California",
            ),
            ("Song - Part 1 - 2020 Remastered", "Song - Part 1"),
            ("  Peter Gabriel (1)  ", "Peter Gabriel (1)"),
        ];

        for (input, expected) in cases {
            let cleaned = cleanup_tags(input);
            assert_eq!(cleaned, expected, "cleanup_tags failed for {input:?}");
        }
    }

    #[test]
    fn largest_artwork_url_selects_tallest_and_handles_empty() {
        assert_eq!(largest_artwork_url(&[]), "");

        let images = [
            ArtworkImage::new("https://img/64", Some(64), Some(64)),
            ArtworkImage::new("https://img/640", Some(640), Some(640)),
            ArtworkImage::new("https://img/300", Some(300), Some(300)),
            ArtworkImage::new("https://img/none", None, None),
        ];
        assert_eq!(largest_artwork_url(&images), "https://img/640");
    }

    proptest! {
        #[test]
        fn prop_merge_array_never_repeats_non_empty_uri(
            left_uris in vec("[a-z]{1,4}", 0..12),
            right_uris in vec("[a-z]{1,4}", 0..12),
        ) {
            let mut left_items = Vec::with_capacity(left_uris.len());
            for id in &left_uris {
                left_items.push(json!({"uri": format!("spotify:track:{id}"), "src": "left"}));
            }
            let mut right_items = Vec::with_capacity(right_uris.len());
            for id in &right_uris {
                right_items.push(json!({"uri": format!("spotify:track:{id}"), "src": "right"}));
            }

            let merged = merge_no_duplicates(&Value::Array(left_items), &Value::Array(right_items));
            let arr = merged.as_array().expect("merged array must be JSON array");
            let mut seen = HashSet::new();
            for elem in arr {
                let uri = elem["uri"].as_str().expect("uri must be string");
                prop_assert!(seen.insert(uri.to_owned()), "duplicate uri {uri}");
            }
        }

        #[test]
        fn prop_merge_preserves_existing_keys_absent_in_incoming(
            keep_val in any::<i64>(),
            new_val in any::<i64>(),
        ) {
            let existing = json!({"image": "https://img", "keep": keep_val, "override": 1});
            let incoming = json!({"override": new_val});
            let merged = merge_no_duplicates(&existing, &incoming);
            prop_assert_eq!(&merged["image"], &json!("https://img"));
            prop_assert_eq!(&merged["keep"], &json!(keep_val));
            prop_assert_eq!(&merged["override"], &json!(new_val));
        }

        #[test]
        fn prop_cleanup_tags_is_idempotent_and_trimmed(raw in ".*") {
            let once = cleanup_tags(&raw);
            let twice = cleanup_tags(&once);
            prop_assert_eq!(&once, &twice);
            prop_assert_eq!(once.trim(), once.as_str());
        }
    }
}
