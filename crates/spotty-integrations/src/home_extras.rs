//! `MaterialSkin` `HomeExtras` descriptors (`HomeExtras.pm`).

/// Descriptor for a `MaterialSkin` home extra widget registered by `Spotty`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HomeExtraDescriptor {
    /// `MaterialSkin` widget identifier (`SpottyExtras<tag>`).
    pub id: &'static str,
    /// Short Spotty extra tag (`spotty`, `home`, `whatsnew`, etc.).
    pub tag: &'static str,
    /// OPML feed `menu` parameter (`home_heroes_<tag>`).
    pub menu_param: &'static str,
    /// Localization token for the home extra title.
    pub title_token: &'static str,
    /// Optional localization token for the home extra subtitle.
    pub subtitle_token: Option<&'static str>,
    /// Whether the home extra requires an active LMS player client.
    pub needs_player: bool,
}

/// Ordered list of the 5 `MaterialSkin` home extras registered in
/// `HomeExtras.pm:44-123`.
pub const HOME_EXTRAS: [HomeExtraDescriptor; 5] = [
    HomeExtraDescriptor {
        id: "SpottyExtrasSpotty",
        tag: "spotty",
        menu_param: "home_heroes_spotty",
        title_token: "PLUGIN_SPOTTY_NAME",
        subtitle_token: Some("PLUGIN_SPOTTY_HOME_EXTRA_SPOTTY_SUBTITLE"),
        needs_player: true,
    },
    HomeExtraDescriptor {
        id: "SpottyExtrashome",
        tag: "home",
        menu_param: "home_heroes_home",
        title_token: "PLUGIN_SPOTTY_HOME_EXTRA_HOME",
        subtitle_token: Some("PLUGIN_SPOTTY_HOME_EXTRA_HOME_SUBTITLE"),
        needs_player: true,
    },
    HomeExtraDescriptor {
        id: "SpottyExtraswhatsnew",
        tag: "whatsnew",
        menu_param: "home_heroes_whatsnew",
        title_token: "PLUGIN_SPOTTY_WHATS_NEW",
        subtitle_token: None,
        needs_player: true,
    },
    HomeExtraDescriptor {
        id: "SpottyExtrastoptracks",
        tag: "toptracks",
        menu_param: "home_heroes_toptracks",
        title_token: "PLUGIN_SPOTTY_TOP_TRACKS",
        subtitle_token: None,
        needs_player: true,
    },
    HomeExtraDescriptor {
        id: "SpottyExtraspopularplaylists",
        tag: "popularplaylists",
        menu_param: "home_heroes_popularplaylists",
        title_token: "PLUGIN_SPOTTY_HOME_EXTRA_POPULAR_PLAYLISTS",
        subtitle_token: None,
        needs_player: true,
    },
];

/// Resolves a `MaterialSkin` home extra `tag` (or widget `id`) to its
/// corresponding OPML `home_heroes_<tag>` menu parameter.
#[must_use]
pub fn resolve_home_extra_menu(tag: &str) -> Option<&'static str> {
    let trimmed = tag.trim();
    for extra in &HOME_EXTRAS {
        if extra.tag == trimmed || extra.id == trimmed {
            return Some(extra.menu_param);
        }
        if extra.tag == "spotty" && trimmed.eq_ignore_ascii_case("SpottyExtrasspotty") {
            return Some(extra.menu_param);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_extras_matches_all_five_material_skin_descriptors() {
        assert_eq!(HOME_EXTRAS.len(), 5, "expected 5 HomeExtras descriptors");

        let expected = [
            (
                "SpottyExtrasSpotty",
                "spotty",
                "home_heroes_spotty",
                "PLUGIN_SPOTTY_NAME",
                Some("PLUGIN_SPOTTY_HOME_EXTRA_SPOTTY_SUBTITLE"),
            ),
            (
                "SpottyExtrashome",
                "home",
                "home_heroes_home",
                "PLUGIN_SPOTTY_HOME_EXTRA_HOME",
                Some("PLUGIN_SPOTTY_HOME_EXTRA_HOME_SUBTITLE"),
            ),
            (
                "SpottyExtraswhatsnew",
                "whatsnew",
                "home_heroes_whatsnew",
                "PLUGIN_SPOTTY_WHATS_NEW",
                None,
            ),
            (
                "SpottyExtrastoptracks",
                "toptracks",
                "home_heroes_toptracks",
                "PLUGIN_SPOTTY_TOP_TRACKS",
                None,
            ),
            (
                "SpottyExtraspopularplaylists",
                "popularplaylists",
                "home_heroes_popularplaylists",
                "PLUGIN_SPOTTY_HOME_EXTRA_POPULAR_PLAYLISTS",
                None,
            ),
        ];

        for (descriptor, (id, tag, menu_param, title_token, subtitle_token)) in
            HOME_EXTRAS.iter().zip(expected)
        {
            assert_eq!(descriptor.id, id, "id mismatch for tag {tag}");
            assert_eq!(descriptor.tag, tag, "tag mismatch for id {id}");
            assert_eq!(
                descriptor.menu_param, menu_param,
                "menu_param mismatch for tag {tag}"
            );
            assert_eq!(
                descriptor.title_token, title_token,
                "title_token mismatch for tag {tag}"
            );
            assert_eq!(
                descriptor.subtitle_token, subtitle_token,
                "subtitle_token mismatch for tag {tag}"
            );
            assert!(
                descriptor.needs_player,
                "needs_player must be true for {tag}"
            );
            assert_eq!(
                resolve_home_extra_menu(tag),
                Some(menu_param),
                "resolve_home_extra_menu({tag}) must return {menu_param}"
            );
            assert_eq!(
                resolve_home_extra_menu(id),
                Some(menu_param),
                "resolve_home_extra_menu({id}) must return {menu_param}"
            );
        }

        assert_eq!(
            resolve_home_extra_menu("unknown"),
            None,
            "unknown tag must return None"
        );
    }
}
