//! Spotty Home priority sorting, top-chart URIs, and legacy icon constants.

/// Track cover fallback icon path.
pub const IMG_TRACK: &str = "/html/images/cover.png";
/// Home menu icon path.
pub const IMG_HOME: &str = "plugins/Spotty/html/images/home.png";
/// Album menu icon path.
pub const IMG_ALBUM: &str = "plugins/Spotty/html/images/album.png";
/// Compilation album icon path.
pub const IMG_COMPILATION: &str = "plugins/Spotty/html/images/compilation.png";
/// Podcast shows icon path.
pub const IMG_PODCAST: &str = "plugins/Spotty/html/images/podcasts.png";
/// Playlist icon path.
pub const IMG_PLAYLIST: &str = "plugins/Spotty/html/images/playlist.png";
/// Collaborative playlist icon path.
pub const IMG_COLLABORATIVE: &str = "plugins/Spotty/html/images/playlist-collab.png";
/// Search menu icon path.
pub const IMG_SEARCH: &str = "plugins/Spotty/html/images/search.png";
/// Account menu icon path.
pub const IMG_ACCOUNT: &str = "plugins/Spotty/html/images/account.png";
/// Artist menu icon path.
pub const IMG_ARTIST: &str = "plugins/Spotty/html/images/artist.png";
/// Follow artist action icon path.
pub const IMG_FOLLOW_ARTIST: &str = "plugins/Spotty/html/images/followartist.png";
/// Top tracks chart icon path (`IMG_TOPTRACKS` in `OPML.pm`).
pub const IMG_TOP_TRACKS: &str = "plugins/Spotty/html/images/toptracks.png";
/// Alias matching `IMG_TOPTRACKS` in `OPML.pm`.
pub const IMG_TOPTRACKS: &str = IMG_TOP_TRACKS;
/// What's New icon path (`IMG_NEWS` in `OPML.pm`).
pub const IMG_WHATS_NEW: &str = "plugins/Spotty/html/images/news.png";
/// Alias matching `IMG_NEWS` in `OPML.pm`.
pub const IMG_NEWS: &str = IMG_WHATS_NEW;
/// Genres & Moods icon path.
pub const IMG_GENRES: &str = "plugins/Spotty/html/images/genres.png";
/// Inbox / featured playlists icon path.
pub const IMG_INBOX: &str = "plugins/Spotty/html/images/inbox.png";
/// Playback transfer icon path.
pub const IMG_TRANSFER: &str = "plugins/Spotty/html/images/transfer.png";
/// Saved songs icon path (`IMG_SONG` in `OPML.pm`).
pub const IMG_SONGS: &str = "plugins/Spotty/html/images/song.png";
/// Alias matching `IMG_SONG` in `OPML.pm`.
pub const IMG_SONG: &str = IMG_SONGS;
/// Radio menu icon path.
pub const IMG_RADIO: &str = "plugins/Spotty/html/images/radio.png";

const TOP_CHART_GLOBAL_XX: &str = "spotify:user:spotifycharts:playlist:37i9dQZEVXbMDoHDwVN2tF";

/// Returns the Spotify Top Tracks chart playlist URI for `country`.
///
/// Matches the 17 country codes in `OPML.pm` case-insensitively and falls
/// back to the `"XX"` global Top 100 chart URI for all other codes.
#[must_use]
pub fn top_chart_uri(country: &str) -> &'static str {
    let trimmed = country.trim();
    if trimmed.eq_ignore_ascii_case("AT") {
        "spotify:user:spotifycharts:playlist:37i9dQZEVXbKNHh6NIXu36"
    } else if trimmed.eq_ignore_ascii_case("AU") {
        "spotify:user:spotifycharts:playlist:37i9dQZEVXbJPcfkRz0wJ0"
    } else if trimmed.eq_ignore_ascii_case("BE") {
        "spotify:user:spotifycharts:playlist:37i9dQZEVXbJNSeeHswcKB"
    } else if trimmed.eq_ignore_ascii_case("CA") {
        "spotify:user:spotifycharts:playlist:37i9dQZEVXbKj23U1GF4IR"
    } else if trimmed.eq_ignore_ascii_case("CH") {
        "spotify:user:spotifycharts:playlist:37i9dQZEVXbJiyhoAPEfMK"
    } else if trimmed.eq_ignore_ascii_case("DE") {
        "spotify:user:spotifycharts:playlist:37i9dQZEVXbJiZcmkrIHGU"
    } else if trimmed.eq_ignore_ascii_case("DK") {
        "spotify:user:spotifycharts:playlist:37i9dQZEVXbL3J0k32lWnN"
    } else if trimmed.eq_ignore_ascii_case("ES") {
        "spotify:user:spotifycharts:playlist:37i9dQZEVXbNFJfN1Vw8d9"
    } else if trimmed.eq_ignore_ascii_case("FI") {
        "spotify:user:spotifycharts:playlist:37i9dQZEVXbMxcczTSoGwZ"
    } else if trimmed.eq_ignore_ascii_case("FR") {
        "spotify:user:spotifycharts:playlist:37i9dQZEVXbIPWwFssbupI"
    } else if trimmed.eq_ignore_ascii_case("GB") {
        "spotify:user:spotifycharts:playlist:37i9dQZEVXbLnolsZ8PSNw"
    } else if trimmed.eq_ignore_ascii_case("IT") {
        "spotify:user:spotifycharts:playlist:37i9dQZEVXbIQnj7RRhdSX"
    } else if trimmed.eq_ignore_ascii_case("NL") {
        "spotify:user:spotifycharts:playlist:37i9dQZEVXbKCF6dqVpDkS"
    } else if trimmed.eq_ignore_ascii_case("NO") {
        "spotify:user:spotifycharts:playlist:37i9dQZEVXbJvfa0Yxg7E7"
    } else if trimmed.eq_ignore_ascii_case("NZ") {
        "spotify:user:spotifycharts:playlist:37i9dQZEVXbM8SIrkERIYl"
    } else if trimmed.eq_ignore_ascii_case("SE") {
        "spotify:user:spotifycharts:playlist:37i9dQZEVXbLoATJ81JYXz"
    } else if trimmed.eq_ignore_ascii_case("US") {
        "spotify:user:spotifycharts:playlist:37i9dQZEVXbLRQDuF5jeBp"
    } else {
        TOP_CHART_GLOBAL_XX
    }
}

/// Computes the strict total-order priority rank for a Home menu item name.
#[must_use]
fn home_item_rank(name: &str) -> u8 {
    let lower = name.to_ascii_lowercase();
    if lower.contains("daily mix") || lower.contains("mixtape") {
        0
    } else if lower.contains("release radar") {
        1
    } else if lower.contains("discover weekly") || lower.contains("mix der woche") {
        2
    } else {
        3
    }
}

/// Sorts Home items in-place using a strict total-order priority rank.
///
/// Priority tiers (`OPML.pm:sortHomeItems`, case-insensitive):
/// 0. `Daily Mix` or `MixTape`
/// 1. `Release Radar`
/// 2. `Discover Weekly` or `Mix der Woche`
/// 3. All remaining items
///
/// Uses a stable sort so items within the same tier preserve their original
/// relative ordering.
pub fn sort_home_items<T, F>(items: &mut [T], name_of: F)
where
    F: Fn(&T) -> &str,
{
    items.sort_by_key(|item| home_item_rank(name_of(item)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_constants_match_legacy_paths() {
        let icons = [
            IMG_TRACK,
            IMG_HOME,
            IMG_ALBUM,
            IMG_COMPILATION,
            IMG_PODCAST,
            IMG_PLAYLIST,
            IMG_COLLABORATIVE,
            IMG_SEARCH,
            IMG_ACCOUNT,
            IMG_ARTIST,
            IMG_FOLLOW_ARTIST,
            IMG_TOP_TRACKS,
            IMG_WHATS_NEW,
            IMG_GENRES,
            IMG_INBOX,
            IMG_TRANSFER,
            IMG_SONGS,
            IMG_RADIO,
        ];
        assert_eq!(icons.len(), 18, "expected 18 IMG_* constants");
        assert_eq!(IMG_TRACK, "/html/images/cover.png");
        assert_eq!(IMG_PLAYLIST, "plugins/Spotty/html/images/playlist.png");
        assert_eq!(IMG_TOP_TRACKS, IMG_TOPTRACKS);
        assert_eq!(IMG_WHATS_NEW, IMG_NEWS);
        assert_eq!(IMG_SONGS, IMG_SONG);
    }

    #[test]
    fn top_chart_uri_covers_all_countries_and_global_fallback() {
        let expected_countries = [
            ("AT", "37i9dQZEVXbKNHh6NIXu36"),
            ("AU", "37i9dQZEVXbJPcfkRz0wJ0"),
            ("BE", "37i9dQZEVXbJNSeeHswcKB"),
            ("CA", "37i9dQZEVXbKj23U1GF4IR"),
            ("CH", "37i9dQZEVXbJiyhoAPEfMK"),
            ("DE", "37i9dQZEVXbJiZcmkrIHGU"),
            ("DK", "37i9dQZEVXbL3J0k32lWnN"),
            ("ES", "37i9dQZEVXbNFJfN1Vw8d9"),
            ("FI", "37i9dQZEVXbMxcczTSoGwZ"),
            ("FR", "37i9dQZEVXbIPWwFssbupI"),
            ("GB", "37i9dQZEVXbLnolsZ8PSNw"),
            ("IT", "37i9dQZEVXbIQnj7RRhdSX"),
            ("NL", "37i9dQZEVXbKCF6dqVpDkS"),
            ("NO", "37i9dQZEVXbJvfa0Yxg7E7"),
            ("NZ", "37i9dQZEVXbM8SIrkERIYl"),
            ("SE", "37i9dQZEVXbLoATJ81JYXz"),
            ("US", "37i9dQZEVXbLRQDuF5jeBp"),
        ];

        for (code, playlist_id) in expected_countries {
            let uri = top_chart_uri(code);
            assert!(
                uri.ends_with(playlist_id),
                "country {code} mapped to unexpected URI {uri}"
            );
            let lower_uri = top_chart_uri(&format!(" {} ", code.to_ascii_lowercase()));
            assert_eq!(
                lower_uri, uri,
                "case/whitespace lookup for {code} mismatched"
            );
        }

        let xx = top_chart_uri("XX");
        assert!(
            xx.ends_with("37i9dQZEVXbMDoHDwVN2tF"),
            "XX fallback unexpected: {xx}"
        );
        assert_eq!(top_chart_uri("JP"), xx, "unknown country should use XX");
        assert_eq!(top_chart_uri(""), xx, "empty country should use XX");
    }

    #[test]
    fn sort_home_items_enforces_total_order_and_preserves_ties() {
        let mut items = vec![
            "Chill Vibes",
            "Discover Weekly",
            "Release Radar",
            "Daily Mix 2",
            "Focus Flow",
            "Dein Mix der Woche",
            "Your MixTape",
            "daily mix 1",
        ];

        sort_home_items(&mut items, |item| item);

        assert_eq!(
            items,
            vec![
                "Daily Mix 2",
                "Your MixTape",
                "daily mix 1",
                "Release Radar",
                "Discover Weekly",
                "Dein Mix der Woche",
                "Chill Vibes",
                "Focus Flow",
            ],
            "home items did not follow total-order priority ranks: {items:?}"
        );
    }
}
