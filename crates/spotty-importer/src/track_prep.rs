//! Track attribute mapping from normalized Spotify models to LMS rows.
//!
//! Mirrors `Plugins::Spotty::Importer::_prepareTrack` (`Importer.pm:458-501`).

use spotty_api::{ArtistSummary, NormalizedTrack};
use spotty_cache::cleanup_tags;
use spotty_types::TagCleanupPolicy;

/// Album release classification mapped to LMS `RELEASETYPE` values.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LmsReleaseType {
    /// Standard full-length album.
    Album,
    /// Single release with at most two tracks and no whole-word `EP` marker.
    Single,
    /// Extended play release (explicit `EP`, `> 2` tracks, or whole-word `EP`).
    Ep,
    /// Multi-artist compilation album.
    Compilation,
}

impl LmsReleaseType {
    /// Returns the uppercase LMS string representation for this release type.
    #[must_use]
    pub const fn as_lms_str(&self) -> &'static str {
        match self {
            Self::Album => "ALBUM",
            Self::Single => "SINGLE",
            Self::Ep => "EP",
            Self::Compilation => "COMPILATION",
        }
    }
}

/// Prepared track row matching LMS `Importer.pm:_prepareTrack` attributes.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LmsTrackRow {
    /// Canonical Spotify track URI (`spotify:track:<id>`).
    pub url: String,
    /// Track title, optionally stripped of edition/remaster suffixes.
    pub title: String,
    /// Joined album-artist (or fallback first track-artist) display names.
    pub artist: String,
    /// Joined Spotify artist URIs corresponding to `artist`, if non-empty.
    pub artist_extid: Option<String>,
    /// Album title, optionally stripped of edition/remaster suffixes.
    pub album: String,
    /// Canonical Spotify album URI (`spotify:album:<id>`).
    pub album_extid: String,
    /// One-based track number within the disc.
    pub tracknum: u32,
    /// One-based disc number, populated only when `max_disc > 1`.
    pub disc: Option<u32>,
    /// Total disc count, populated only when `max_disc > 1`.
    pub discc: Option<u32>,
    /// Track duration in whole seconds (`duration_ms / 1000`).
    pub secs: u32,
    /// Four-digit release year parsed from `release_date`, if valid.
    pub year: Option<u32>,
    /// Album or fallback track artwork URL.
    pub cover: String,
    /// External LMS identifier (`spotify:track:<id>`).
    pub extid: String,
    /// `true` for compilation albums with more than one album artist.
    pub compilation: bool,
    /// LMS release type (`ALBUM`, `SINGLE`, `EP`, or `COMPILATION`).
    pub release_type: LmsReleaseType,
    /// Unix epoch timestamp parsed from `added_at`, or `0` when unavailable.
    pub timestamp: i64,
    /// LMS content type (`"spt"` for Spotify tracks).
    pub content_type: String,
    /// LMS genre label (`"Spotify"`).
    pub genre: String,
    /// Optional multi-account tag (`"spotify:<userId>"`).
    pub account_tag: Option<String>,
}

/// Converts a [`NormalizedTrack`] into an [`LmsTrackRow`].
///
/// Mirrors `Plugins::Spotty::Importer::_prepareTrack` (`Importer.pm:458-501`):
/// uses `cached_track` when provided, joins album-artist names and URIs with
/// `split_sep` (falling back to the first track artist), applies
/// `cleanup_tags` under [`TagCleanupPolicy::StripSuffixes`], promotes
/// `"single"` to [`LmsReleaseType::Ep`] when `album_track_count > 2` or the
/// album title contains whole-word `EP`, populates `disc` and `discc` only
/// when `max_disc > 1`, computes `secs = duration_ms / 1000`, parses the
/// 4-digit release year and ISO-8601 `added_at` timestamp, and sets
/// `content_type = "spt"` and `genre = "Spotify"`.
#[must_use]
pub fn prepare_lms_track(
    track: &NormalizedTrack,
    cached_track: Option<&NormalizedTrack>,
    max_disc: u32,
    album_track_count: u32,
    split_sep: char,
    tag_cleanup: TagCleanupPolicy,
    account_tag: Option<&str>,
) -> LmsTrackRow {
    let item = cached_track.unwrap_or(track);

    let artist_source: &[ArtistSummary] = if !item.album.artists.is_empty() {
        &item.album.artists
    } else if let Some(first_artist) = item.artists.first() {
        std::slice::from_ref(first_artist)
    } else {
        &[]
    };

    let artist = join_artist_field(artist_source, split_sep, |summary| summary.name.as_str());
    let joined_extids = join_artist_field(artist_source, split_sep, |summary| summary.uri.as_str());
    let artist_extid = if joined_extids.is_empty() {
        None
    } else {
        Some(joined_extids)
    };

    let release_type =
        classify_release_type(&item.album.album_type, &item.album.name, album_track_count);

    let (title, album) = match tag_cleanup {
        TagCleanupPolicy::StripSuffixes => {
            (cleanup_tags(&item.name), cleanup_tags(&item.album.name))
        }
        TagCleanupPolicy::PreserveRaw => (item.name.clone(), item.album.name.clone()),
        _ => (item.name.clone(), item.album.name.clone()),
    };

    let (disc, discc) = if max_disc > 1 {
        (Some(item.disc_number), Some(max_disc))
    } else {
        (None, None)
    };

    let secs = u32::try_from(item.duration_ms / 1000).unwrap_or(u32::MAX);
    let year = parse_release_year(&item.album.release_date);

    let cover = if item.album.image.is_empty() {
        item.image.clone()
    } else {
        item.album.image.clone()
    };

    let compilation =
        item.album.album_type.eq_ignore_ascii_case("compilation") && item.album.artists.len() > 1;

    let timestamp = item
        .album
        .added_at
        .as_deref()
        .or(item.added_at.as_deref())
        .and_then(parse_iso8601_to_epoch)
        .unwrap_or(0);

    LmsTrackRow {
        url: item.uri.clone(),
        title,
        artist,
        artist_extid,
        album,
        album_extid: item.album.uri.clone(),
        tracknum: item.track_number,
        disc,
        discc,
        secs,
        year,
        cover,
        extid: item.uri.clone(),
        compilation,
        release_type,
        timestamp,
        content_type: String::from("spt"),
        genre: String::from("Spotify"),
        account_tag: account_tag.map(str::to_owned),
    }
}

fn join_artist_field<'a>(
    artists: &'a [ArtistSummary],
    split_sep: char,
    extract: fn(&'a ArtistSummary) -> &'a str,
) -> String {
    let mut out = String::new();
    for artist in artists {
        let value = extract(artist);
        if value.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(split_sep);
        }
        out.push_str(value);
    }
    out
}

fn classify_release_type(
    album_type: &str,
    album_name: &str,
    album_track_count: u32,
) -> LmsReleaseType {
    if album_type.eq_ignore_ascii_case("compilation") {
        return LmsReleaseType::Compilation;
    }
    if album_type.eq_ignore_ascii_case("ep") {
        return LmsReleaseType::Ep;
    }
    if contains_ascii_case_insensitive(album_type, "single") {
        if album_track_count > 2 || contains_whole_word_ep(album_name) {
            return LmsReleaseType::Ep;
        }
        return LmsReleaseType::Single;
    }
    LmsReleaseType::Album
}

fn contains_ascii_case_insensitive(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let hay = haystack.as_bytes();
    let pat = needle.as_bytes();
    if hay.len() < pat.len() {
        return false;
    }
    for window in hay.windows(pat.len()) {
        if window.eq_ignore_ascii_case(pat) {
            return true;
        }
    }
    false
}

fn contains_whole_word_ep(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut idx = 0;
    while let Some(rel) = text[idx..].find("EP") {
        let start = idx + rel;
        let end = start + 2;
        let left_ok = start == 0 || !is_word_byte(bytes[start - 1]);
        let right_ok = end == bytes.len() || !is_word_byte(bytes[end]);
        if left_ok && right_ok {
            return true;
        }
        idx = start + 1;
    }
    false
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn parse_release_year(release_date: &str) -> Option<u32> {
    let prefix = release_date.get(..4)?;
    if !prefix.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    prefix.parse::<u32>().ok()
}

fn parse_iso8601_to_epoch(raw: &str) -> Option<i64> {
    let s = raw.trim();
    if s.len() < 10 {
        return None;
    }
    let bytes = s.as_bytes();
    if bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }

    let year = parse_u32_slice(s.get(0..4)?)?;
    let month = parse_u32_slice(s.get(5..7)?)?;
    let day = parse_u32_slice(s.get(8..10)?)?;
    if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
        return None;
    }

    let (hour, minute, second, tz_offset_secs) = if s.len() == 10 {
        (0_i64, 0_i64, 0_i64, 0_i64)
    } else {
        let sep = bytes[10];
        if sep != b'T' && sep != b't' && sep != b' ' {
            return None;
        }
        if s.len() < 19 || bytes[13] != b':' || bytes[16] != b':' {
            return None;
        }
        let h = i64::from(parse_u32_slice(s.get(11..13)?)?);
        let m = i64::from(parse_u32_slice(s.get(14..16)?)?);
        let sec = i64::from(parse_u32_slice(s.get(17..19)?)?);
        if h > 23 || m > 59 || sec > 60 {
            return None;
        }

        let mut rest = s.get(19..)?;
        if let Some(after_dot) = rest.strip_prefix('.') {
            let frac_len = after_dot.bytes().take_while(u8::is_ascii_digit).count();
            if frac_len == 0 {
                return None;
            }
            rest = after_dot.get(frac_len..)?;
        }

        let offset = parse_tz_offset_secs(rest)?;
        (h, m, sec.min(59), offset)
    };

    let days = days_from_civil(i64::from(year), month, day);
    let secs = days
        .checked_mul(86_400)?
        .checked_add(hour * 3_600 + minute * 60 + second)?
        .checked_sub(tz_offset_secs)?;
    Some(secs)
}

fn parse_tz_offset_secs(suffix: &str) -> Option<i64> {
    if suffix.is_empty() || suffix == "Z" || suffix == "z" {
        return Some(0);
    }
    let (sign, rest) = if let Some(r) = suffix.strip_prefix('+') {
        (1_i64, r)
    } else {
        let r = suffix.strip_prefix('-')?;
        (-1_i64, r)
    };

    let (tz_h, tz_m) = if rest.len() == 5 && rest.as_bytes()[2] == b':' {
        (
            parse_u32_slice(rest.get(0..2)?)?,
            parse_u32_slice(rest.get(3..5)?)?,
        )
    } else if rest.len() == 4 {
        (
            parse_u32_slice(rest.get(0..2)?)?,
            parse_u32_slice(rest.get(2..4)?)?,
        )
    } else if rest.len() == 2 {
        (parse_u32_slice(rest)?, 0)
    } else {
        return None;
    };

    if tz_h > 23 || tz_m > 59 {
        return None;
    }
    Some(sign * (i64::from(tz_h) * 3_600 + i64::from(tz_m) * 60))
}

fn parse_u32_slice(digits: &str) -> Option<u32> {
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u32>().ok()
}

fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap_year(year) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

fn is_leap_year(year: u32) -> bool {
    (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = i64::from(month);
    let d = i64::from(day);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use spotty_api::MinAlbum;

    use super::*;

    fn sample_track() -> NormalizedTrack {
        let artists = vec![
            ArtistSummary::new("a1", "spotify:artist:a1", "Alpha"),
            ArtistSummary::new("a2", "spotify:artist:a2", "Beta"),
        ];
        let mut album = MinAlbum::new(
            "alb1",
            "spotify:album:alb1",
            "Midnight Horizon (Deluxe Edition)",
            artists.clone(),
            "https://img.example/cover.jpg",
            "2024-05-17",
            "album",
            Some(String::from("2026-01-10T12:00:00Z")),
        );
        album.added_at = Some(String::from("2026-01-10T12:00:00Z"));

        let mut track = NormalizedTrack::default();
        track.id = String::from("t1");
        track.uri = String::from("spotify:track:t1");
        track.name = String::from("Starlight - 2024 Remaster");
        track.artists = artists;
        track.album = album;
        track.duration_ms = 245_900;
        track.track_number = 3;
        track.disc_number = 2;
        track.is_playable = true;
        track.image = String::from("https://img.example/cover.jpg");
        track
    }

    #[test]
    fn prepare_lms_track_joins_multi_artists_cleans_tags_and_parses_dates() {
        let track = sample_track();
        let row = prepare_lms_track(
            &track,
            None,
            2,
            10,
            ';',
            TagCleanupPolicy::StripSuffixes,
            Some("spotify:user1"),
        );

        assert_eq!(row.url, "spotify:track:t1");
        assert_eq!(row.title, "Starlight");
        assert_eq!(row.album, "Midnight Horizon");
        assert_eq!(row.artist, "Alpha;Beta");
        assert_eq!(
            row.artist_extid.as_deref(),
            Some("spotify:artist:a1;spotify:artist:a2")
        );
        assert_eq!(row.album_extid, "spotify:album:alb1");
        assert_eq!(row.tracknum, 3);
        assert_eq!(row.disc, Some(2));
        assert_eq!(row.discc, Some(2));
        assert_eq!(row.secs, 245);
        assert_eq!(row.year, Some(2024));
        assert_eq!(row.cover, "https://img.example/cover.jpg");
        assert_eq!(row.extid, "spotify:track:t1");
        assert!(!row.compilation);
        assert_eq!(row.release_type, LmsReleaseType::Album);
        assert_eq!(row.release_type.as_lms_str(), "ALBUM");
        assert_eq!(row.timestamp, 1_768_046_400);
        assert_eq!(row.content_type, "spt");
        assert_eq!(row.genre, "Spotify");
        assert_eq!(row.account_tag.as_deref(), Some("spotify:user1"));
    }

    #[test]
    fn prepare_lms_track_preserves_raw_tags_when_configured() {
        let track = sample_track();
        let row = prepare_lms_track(
            &track,
            None,
            1,
            10,
            ',',
            TagCleanupPolicy::PreserveRaw,
            None,
        );

        assert_eq!(row.title, "Starlight - 2024 Remaster");
        assert_eq!(row.album, "Midnight Horizon (Deluxe Edition)");
        assert_eq!(row.artist, "Alpha,Beta");
        assert_eq!(row.disc, None);
        assert_eq!(row.discc, None);
        assert_eq!(row.account_tag, None);
    }

    #[test]
    fn single_promotes_to_ep_by_track_count_or_whole_word_ep() {
        let mut single = sample_track();
        single.album.album_type = String::from("single");
        single.album.name = String::from("Summer");

        let short_single = prepare_lms_track(
            &single,
            None,
            1,
            2,
            ';',
            TagCleanupPolicy::PreserveRaw,
            None,
        );
        assert_eq!(short_single.release_type, LmsReleaseType::Single);
        assert_eq!(short_single.release_type.as_lms_str(), "SINGLE");

        let three_track_ep = prepare_lms_track(
            &single,
            None,
            1,
            3,
            ';',
            TagCleanupPolicy::PreserveRaw,
            None,
        );
        assert_eq!(three_track_ep.release_type, LmsReleaseType::Ep);
        assert_eq!(three_track_ep.release_type.as_lms_str(), "EP");

        single.album.name = String::from("Summer EP");
        let named_ep = prepare_lms_track(
            &single,
            None,
            1,
            1,
            ';',
            TagCleanupPolicy::PreserveRaw,
            None,
        );
        assert_eq!(named_ep.release_type, LmsReleaseType::Ep);

        single.album.name = String::from("STEP");
        let non_ep_word = prepare_lms_track(
            &single,
            None,
            1,
            2,
            ';',
            TagCleanupPolicy::PreserveRaw,
            None,
        );
        assert_eq!(non_ep_word.release_type, LmsReleaseType::Single);
    }

    #[test]
    fn disc_and_discc_are_gated_strictly_on_max_disc_gt_one() {
        let track = sample_track();

        let single_disc = prepare_lms_track(
            &track,
            None,
            1,
            10,
            ';',
            TagCleanupPolicy::PreserveRaw,
            None,
        );
        assert_eq!(single_disc.disc, None);
        assert_eq!(single_disc.discc, None);

        let multi_disc = prepare_lms_track(
            &track,
            None,
            3,
            10,
            ';',
            TagCleanupPolicy::PreserveRaw,
            None,
        );
        assert_eq!(multi_disc.disc, Some(2));
        assert_eq!(multi_disc.discc, Some(3));
    }

    #[test]
    fn compilation_requires_multiple_album_artists_and_cached_track_wins() {
        let raw = sample_track();
        let mut cached = sample_track();
        cached.name = String::from("Cached Override");
        cached.album.album_type = String::from("compilation");

        let row = prepare_lms_track(
            &raw,
            Some(&cached),
            1,
            12,
            ';',
            TagCleanupPolicy::PreserveRaw,
            None,
        );
        assert_eq!(row.title, "Cached Override");
        assert!(row.compilation);
        assert_eq!(row.release_type, LmsReleaseType::Compilation);
        assert_eq!(row.release_type.as_lms_str(), "COMPILATION");

        cached.album.artists.truncate(1);
        let single_artist_comp = prepare_lms_track(
            &raw,
            Some(&cached),
            1,
            12,
            ';',
            TagCleanupPolicy::PreserveRaw,
            None,
        );
        assert!(!single_artist_comp.compilation);
    }

    #[test]
    fn falls_back_to_first_track_artist_and_handles_offsets_and_invalid_dates() {
        let mut track = sample_track();
        track.album.artists.clear();
        track.album.release_date = String::from("bad");
        track.album.added_at = Some(String::from("2026-01-10T14:00:00+02:00"));

        let row = prepare_lms_track(&track, None, 1, 5, ';', TagCleanupPolicy::PreserveRaw, None);
        assert_eq!(row.artist, "Alpha");
        assert_eq!(row.artist_extid.as_deref(), Some("spotify:artist:a1"));
        assert_eq!(row.year, None);
        assert_eq!(row.timestamp, 1_768_046_400);

        track.album.added_at = Some(String::from("2023-02-29T00:00:00Z"));
        let invalid_leap =
            prepare_lms_track(&track, None, 1, 5, ';', TagCleanupPolicy::PreserveRaw, None);
        assert_eq!(invalid_leap.timestamp, 0);
    }
}
