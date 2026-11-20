//! LMS `strings.txt` localization parser and lookup catalog.

use std::collections::HashMap;

/// Default fallback language code used when a requested locale is absent.
const FALLBACK_LANG: &str = "EN";

/// In-memory catalog of LMS `strings.txt` localization tokens and translations.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StringCatalog {
    entries: HashMap<String, HashMap<String, String>>,
}

impl StringCatalog {
    /// Creates an empty localization catalog.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Parses LMS `strings.txt` content into a [`StringCatalog`].
    ///
    /// Top-level non-indented lines define uppercase token keys, followed by
    /// tab-indented `<LANG>\t<text>` entries. Blank lines and `#` comments are
    /// ignored, multi-tab separators between language code and text are
    /// collapsed, and `\n` escape sequences are unescaped to newlines.
    #[must_use]
    pub fn parse_strings_txt(input: &str) -> Self {
        let mut entries: HashMap<String, HashMap<String, String>> = HashMap::new();
        let mut current_token: Option<String> = None;

        for raw_line in input.lines() {
            let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }

            if line.starts_with(['\t', ' ']) {
                let Some(token) = current_token.as_deref() else {
                    continue;
                };
                let after_indent = line.trim_start_matches(['\t', ' ']);
                let Some((lang, rest)) = after_indent.split_once('\t') else {
                    continue;
                };
                let lang_trimmed = lang.trim();
                if lang_trimmed.is_empty() {
                    continue;
                }
                let raw_text = rest.trim_start_matches('\t');
                let text = unescape_newlines(raw_text);
                let translations = entries.entry(token.to_owned()).or_default();
                translations.insert(lang_trimmed.to_ascii_uppercase(), text);
                continue;
            }

            if trimmed.chars().any(char::is_whitespace) {
                continue;
            }

            entries.entry(trimmed.to_owned()).or_default();
            current_token = Some(trimmed.to_owned());
        }

        Self { entries }
    }

    /// Looks up localized text for `token` in `lang`.
    ///
    /// Lookup is case-insensitive on `lang`, falling back to `EN` if the
    /// requested language is unavailable, and finally to `token` itself when
    /// neither `token` nor an `EN` translation exists.
    #[must_use]
    pub fn text<'a>(&'a self, token: &'a str, lang: &str) -> &'a str {
        let Some(translations) = self.entries.get(token) else {
            return token;
        };

        let normalized_lang = lang.to_ascii_uppercase();
        if let Some(localized) = translations.get(&normalized_lang) {
            return localized.as_str();
        }

        if let Some(fallback) = translations.get(FALLBACK_LANG) {
            return fallback.as_str();
        }

        token
    }

    /// Returns `true` if `token` is present in the catalog.
    #[must_use]
    pub fn has_token(&self, token: &str) -> bool {
        self.entries.contains_key(token)
    }

    /// Returns the number of distinct localization tokens in the catalog.
    #[must_use]
    pub fn token_count(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` if the catalog contains no localization tokens.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Converts literal `\n` escape sequences in LMS string values to newlines.
fn unescape_newlines(raw: &str) -> String {
    if raw.contains("\\n") {
        raw.replace("\\n", "\n")
    } else {
        raw.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REAL_STRINGS_TXT: &str = include_str!("../../../strings.txt");

    #[test]
    fn parses_synthetic_catalog_with_comments_multitabs_and_newlines() {
        let input = "\
# Leading comment
PLUGIN_SYNTH_GREETING
\t\tEN\t\tHello\\nWorld
\tde\tHallo\\nWelt

# Indented comment should be ignored
\t# Inline comment
PLUGIN_SYNTH_ONLY_NL
\tNL\tAlleen Nederlands
";

        let catalog = StringCatalog::parse_strings_txt(input);
        assert!(
            !catalog.is_empty(),
            "parsed synthetic catalog should not be empty"
        );
        assert_eq!(
            catalog.token_count(),
            2,
            "expected 2 tokens in synthetic catalog, got {}",
            catalog.token_count()
        );
        assert!(
            catalog.has_token("PLUGIN_SYNTH_GREETING"),
            "missing PLUGIN_SYNTH_GREETING token"
        );
        assert_eq!(
            catalog.text("PLUGIN_SYNTH_GREETING", "en"),
            "Hello\nWorld",
            "lowercase lang lookup should unescape \\n"
        );
        assert_eq!(
            catalog.text("PLUGIN_SYNTH_GREETING", "DE"),
            "Hallo\nWelt",
            "case-insensitive DE lookup failed"
        );
        assert_eq!(
            catalog.text("PLUGIN_SYNTH_GREETING", "fr"),
            "Hello\nWorld",
            "missing FR should fall back to EN"
        );
        assert_eq!(
            catalog.text("PLUGIN_SYNTH_ONLY_NL", "FR"),
            "PLUGIN_SYNTH_ONLY_NL",
            "missing both FR and EN should fall back to token itself"
        );
        assert_eq!(
            catalog.text("NON_EXISTENT_TOKEN", "EN"),
            "NON_EXISTENT_TOKEN",
            "unknown token should fall back to token itself"
        );
    }

    #[test]
    fn ignores_orphan_translations_before_first_token() {
        let input = "\
\tEN\tOrphan translation
PLUGIN_AFTER_ORPHAN
\tEN\tValid text
";
        let catalog = StringCatalog::parse_strings_txt(input);
        assert_eq!(
            catalog.token_count(),
            1,
            "orphan translation before any token should not create an entry"
        );
        assert_eq!(catalog.text("PLUGIN_AFTER_ORPHAN", "en"), "Valid text");
    }

    #[test]
    fn parses_real_repository_strings_txt() {
        let catalog = StringCatalog::parse_strings_txt(REAL_STRINGS_TXT);

        assert!(
            catalog.token_count() > 50,
            "expected real strings.txt to have > 50 tokens, got {}",
            catalog.token_count()
        );
        assert!(
            catalog.has_token("PLUGIN_SPOTTY_NAME"),
            "real strings.txt should contain PLUGIN_SPOTTY_NAME"
        );
        assert_eq!(
            catalog.text("PLUGIN_SPOTTY_NAME", "en"),
            "Spotty",
            "PLUGIN_SPOTTY_NAME EN mismatch"
        );
        assert_eq!(
            catalog.text("PLUGIN_SPOTTY_NAME", "de"),
            "Spotty",
            "PLUGIN_SPOTTY_NAME has no DE entry and should fall back to EN"
        );
        assert_eq!(
            catalog.text("PLUGIN_SPOTTY_NAME", "NL"),
            "Spotty Spotify",
            "PLUGIN_SPOTTY_NAME NL mismatch"
        );

        assert!(
            catalog.has_token("PLUGIN_SPOTTY_ERROR_429"),
            "real strings.txt should contain PLUGIN_SPOTTY_ERROR_429"
        );
        assert_eq!(
            catalog.text("PLUGIN_SPOTTY_ERROR_429", "EN"),
            "Rate limit exceeded"
        );
        assert_eq!(
            catalog.text("PLUGIN_SPOTTY_ERROR_429", "de"),
            "Zugriffsrate überschritten"
        );
        assert_eq!(
            catalog.text("PLUGIN_SPOTTY_ERROR_429", "FR"),
            "Limite de débit dépassée"
        );
        assert_eq!(
            catalog.text("PLUGIN_SPOTTY_ERROR_429", "JA"),
            "Rate limit exceeded",
            "missing JA translation should fall back to EN"
        );
    }
}
