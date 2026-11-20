//! State-machine parser for desktop Spotify `Storage/*.file` folder blobs.

use std::collections::HashMap;
use std::str;

use serde::Deserialize;
use serde::Serialize;
use spotty_types::Result;
use spotty_types::SpottyError;

/// Maximum permitted size of a Spotify `Storage/*.file` folder blob (512 KiB).
pub const MAX_FOLDER_FILE_BYTES: usize = 512 * 1024;

const SPOTIFY_URI_PREFIX: &[u8] = b"spotify:";
const PLAYLIST_ID_LEN: usize = 22;

/// Parsed entry from a Spotify playlist folder storage file.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FolderEntry {
    /// A playlist folder (`start-group`) node.
    Folder {
        /// Opaque folder identifier from the Spotify cache blob.
        id: String,
        /// Decoded folder display name.
        name: String,
        /// Parent folder identifier, or `"/"` for the root level.
        parent: String,
        /// Zero-based sequential order within the storage file.
        order: usize,
    },
    /// A playlist reference (`spotify:playlist:<id>`) entry.
    Playlist {
        /// Canonical `spotify:playlist:<22-char-id>` URI.
        uri: String,
        /// Parent folder identifier, or `"/"` for the root level.
        parent: String,
        /// Zero-based sequential order within the storage file.
        order: usize,
    },
}

impl FolderEntry {
    /// Returns the parent folder identifier (`"/"` for root-level entries).
    #[must_use]
    pub fn parent(&self) -> &str {
        match self {
            Self::Folder { parent, .. } | Self::Playlist { parent, .. } => parent,
        }
    }

    /// Returns the zero-based sequential order from the storage file.
    #[must_use]
    pub fn order(&self) -> usize {
        match self {
            Self::Folder { order, .. } | Self::Playlist { order, .. } => *order,
        }
    }
}

/// Parses a binary or text Spotify `Storage/*.file` blob into a hierarchy map.
///
/// Enforces `bytes.len() <= MAX_FOLDER_FILE_BYTES`, verifies the presence of a
/// `start-group` marker, splits on `spotify:[pse]`, truncates tokens at `\r`,
/// tracks nested group parents on a stack, and URL-unescapes folder names with
/// UTF-8 and Latin-1 fallback decoding.
///
/// # Errors
///
/// Returns [`SpottyError::Cache`] if `bytes` exceeds [`MAX_FOLDER_FILE_BYTES`]
/// or does not contain a `start-group` word marker.
pub fn parse_folder_blob(bytes: &[u8]) -> Result<HashMap<String, FolderEntry>> {
    if bytes.len() > MAX_FOLDER_FILE_BYTES {
        return Err(SpottyError::Cache(format!(
            "folder blob exceeds {} bytes ({} bytes)",
            MAX_FOLDER_FILE_BYTES,
            bytes.len()
        )));
    }

    if !contains_word(bytes, b"start-group") {
        return Err(SpottyError::Cache(String::from(
            "folder blob does not contain a start-group marker",
        )));
    }

    let mut entries = HashMap::new();
    let mut stack: Vec<String> = Vec::new();
    let mut parent = String::from("/");
    let mut order: usize = 0;

    for raw_item in split_spotify_tokens(bytes) {
        let item = truncate_at_cr(raw_item);

        if let Some(rest) = item.strip_prefix(b"laylist:") {
            if let Some(id) = parse_playlist_id(rest) {
                let uri = format!("spotify:playlist:{id}");
                entries.insert(
                    uri.clone(),
                    FolderEntry::Playlist {
                        uri,
                        parent: parent.clone(),
                        order,
                    },
                );
                order += 1;
            }
        } else if item.starts_with(b"tart-group") {
            let Some((folder_id, name)) = parse_start_group(item) else {
                continue;
            };
            entries.insert(
                folder_id.clone(),
                FolderEntry::Folder {
                    id: folder_id.clone(),
                    name,
                    parent: parent.clone(),
                    order,
                },
            );
            order += 1;
            stack.push(parent);
            parent = folder_id;
        } else if item.starts_with(b"nd-group") {
            parent = stack.pop().unwrap_or_else(|| String::from("/"));
        }
    }

    Ok(entries)
}

/// Checks whether `haystack` contains `needle` bounded by non-word ASCII bytes.
pub(crate) fn contains_word(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }

    let mut pos = 0;
    while pos + needle.len() <= haystack.len() {
        if &haystack[pos..pos + needle.len()] == needle {
            let left_ok = pos == 0 || !is_ascii_word_byte(haystack[pos - 1]);
            let right_idx = pos + needle.len();
            let right_ok = right_idx == haystack.len() || !is_ascii_word_byte(haystack[right_idx]);
            if left_ok && right_ok {
                return true;
            }
        }
        pos += 1;
    }

    false
}

fn is_ascii_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn split_spotify_tokens(bytes: &[u8]) -> Vec<&[u8]> {
    let mut tokens = Vec::new();
    let delim_len = SPOTIFY_URI_PREFIX.len() + 1;
    let mut idx = 0;
    let mut segment_start: Option<usize> = None;

    while idx + delim_len <= bytes.len() {
        if &bytes[idx..idx + SPOTIFY_URI_PREFIX.len()] == SPOTIFY_URI_PREFIX {
            let tag = bytes[idx + SPOTIFY_URI_PREFIX.len()];
            if matches!(tag, b'p' | b's' | b'e') {
                if let Some(start) = segment_start {
                    tokens.push(&bytes[start..idx]);
                }
                segment_start = Some(idx + delim_len);
                idx += delim_len;
                continue;
            }
        }
        idx += 1;
    }

    if let Some(start) = segment_start {
        tokens.push(&bytes[start..]);
    }

    tokens
}

fn truncate_at_cr(bytes: &[u8]) -> &[u8] {
    for (idx, &byte) in bytes.iter().enumerate() {
        if byte == b'\r' {
            return &bytes[..idx];
        }
    }
    bytes
}

fn parse_playlist_id(rest: &[u8]) -> Option<&str> {
    if rest.len() < PLAYLIST_ID_LEN {
        return None;
    }
    let candidate = &rest[..PLAYLIST_ID_LEN];
    if !candidate.iter().all(u8::is_ascii_alphanumeric) {
        return None;
    }
    str::from_utf8(candidate).ok()
}

fn parse_start_group(item: &[u8]) -> Option<(String, String)> {
    let mut parts: Vec<&[u8]> = Vec::new();
    let mut start = 0;
    for (idx, &byte) in item.iter().enumerate() {
        if byte == b':' {
            parts.push(&item[start..idx]);
            start = idx + 1;
        }
    }
    parts.push(&item[start..]);

    if parts.len() < 3 {
        return None;
    }

    let raw_id = parts[parts.len() - 2];
    let raw_name = parts[parts.len() - 1];
    if raw_id.is_empty() {
        return None;
    }

    let id = String::from_utf8_lossy(raw_id).into_owned();
    let name = decode_folder_name(raw_name);
    Some((id, name))
}

fn decode_folder_name(raw: &[u8]) -> String {
    let unescaped = percent_unescape_and_space(raw);
    match String::from_utf8(unescaped) {
        Ok(valid_utf8) => valid_utf8,
        Err(err) => {
            let bytes = err.into_bytes();
            let mut latin1 = String::with_capacity(bytes.len());
            for byte in bytes {
                latin1.push(char::from(byte));
            }
            latin1
        }
    }
}

fn percent_unescape_and_space(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    let mut idx = 0;

    while idx < raw.len() {
        if raw[idx] == b'%' && idx + 2 < raw.len() {
            if let (Some(hi), Some(lo)) = (hex_val(raw[idx + 1]), hex_val(raw[idx + 2])) {
                let decoded = (hi << 4) | lo;
                out.push(if decoded == b'+' { b' ' } else { decoded });
                idx += 3;
                continue;
            }
        }
        let byte = raw[idx];
        out.push(if byte == b'+' { b' ' } else { byte });
        idx += 1;
    }

    out
}

fn hex_val(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_start_end_groups_and_playlists() {
        let blob = b"\x00\x01hdr\r\
            spotify:start-group:f001:Rock+%26+Roll\r\x02\
            spotify:playlist:37i9dQZF1DXcBWIGoYBM5M\r\
            spotify:start-group:f002:90s+Alt\r\
            spotify:playlist:1111111111111111111111\r\
            spotify:end-group:f002\r\
            spotify:end-group:f001\r\
            spotify:playlist:2222222222222222222222\r";

        let parsed = parse_folder_blob(blob).expect("valid nested folder blob should parse");
        assert_eq!(parsed.len(), 5, "expected 2 folders and 3 playlists");

        assert_eq!(
            parsed.get("f001"),
            Some(&FolderEntry::Folder {
                id: String::from("f001"),
                name: String::from("Rock & Roll"),
                parent: String::from("/"),
                order: 0,
            })
        );
        assert_eq!(
            parsed.get("spotify:playlist:37i9dQZF1DXcBWIGoYBM5M"),
            Some(&FolderEntry::Playlist {
                uri: String::from("spotify:playlist:37i9dQZF1DXcBWIGoYBM5M"),
                parent: String::from("f001"),
                order: 1,
            })
        );
        assert_eq!(
            parsed.get("f002"),
            Some(&FolderEntry::Folder {
                id: String::from("f002"),
                name: String::from("90s Alt"),
                parent: String::from("f001"),
                order: 2,
            })
        );
        assert_eq!(
            parsed.get("spotify:playlist:1111111111111111111111"),
            Some(&FolderEntry::Playlist {
                uri: String::from("spotify:playlist:1111111111111111111111"),
                parent: String::from("f002"),
                order: 3,
            })
        );
        assert_eq!(
            parsed.get("spotify:playlist:2222222222222222222222"),
            Some(&FolderEntry::Playlist {
                uri: String::from("spotify:playlist:2222222222222222222222"),
                parent: String::from("/"),
                order: 4,
            })
        );
    }

    #[test]
    fn decodes_utf8_and_latin1_folder_names() {
        let blob = b"spotify:start-group:utf8id:Caf%C3%A9+Hits\r\
            spotify:end-group:utf8id\r\
            spotify:start-group:lat1id:M%FCnchen+Mix\r\
            spotify:end-group:lat1id\r";

        let parsed = parse_folder_blob(blob).expect("UTF-8 and Latin-1 names should decode");
        assert_eq!(
            parsed.get("utf8id"),
            Some(&FolderEntry::Folder {
                id: String::from("utf8id"),
                name: String::from("Caf\u{00e9} Hits"),
                parent: String::from("/"),
                order: 0,
            })
        );
        assert_eq!(
            parsed.get("lat1id"),
            Some(&FolderEntry::Folder {
                id: String::from("lat1id"),
                name: String::from("M\u{00fc}nchen Mix"),
                parent: String::from("/"),
                order: 1,
            })
        );
    }

    #[test]
    fn rejects_oversized_blob_and_missing_start_group() {
        let oversized = vec![b'a'; MAX_FOLDER_FILE_BYTES + 1];
        let size_err = parse_folder_blob(&oversized);
        assert!(size_err.is_err(), "blobs > 512 KiB must fail");

        let flat = b"spotify:playlist:37i9dQZF1DXcBWIGoYBM5M\r";
        let flat_err = parse_folder_blob(flat);
        assert!(flat_err.is_err(), "blobs without start-group must fail");
    }

    #[test]
    fn ignores_short_or_invalid_playlist_ids() {
        let blob = b"spotify:start-group:f1:List\r\
            spotify:playlist:too_short\r\
            spotify:playlist:aaaaaaaaaaaaaaaaaaaaaa_trailing\r\
            spotify:end-group:f1\r";

        let parsed = parse_folder_blob(blob).expect("blob with valid start-group should parse");
        assert_eq!(parsed.len(), 2, "expected folder f1 and 1 valid playlist");
        assert!(
            parsed.contains_key("spotify:playlist:aaaaaaaaaaaaaaaaaaaaaa"),
            "22-char alphanumeric prefix should be recorded"
        );
    }
}
