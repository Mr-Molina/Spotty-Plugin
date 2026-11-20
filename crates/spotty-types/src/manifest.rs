//! Plugin manifest (`install.xml`) parsing and Spotify client `IconCode`
//! validation.

use std::fmt;
use std::str::FromStr;

use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;

use crate::error::Result;
use crate::error::SpottyError;

/// Default Spotty Spotify application client icon hash bundled in
/// `install.xml`.
pub const DEFAULT_ICON_HASH: &str = "93aac68fb06348598c1e67734dfaceee";

/// Validated 32-character lowercase ASCII hexadecimal Spotify client icon code.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct IconCode(String);

impl IconCode {
    /// Validates a 32-character ASCII hex string (`^[a-fA-F0-9]{32}$`) and
    /// normalizes it to lowercase.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Io`] if `raw` is not exactly 32 ASCII hexadecimal
    /// digits.
    pub fn new(raw: impl AsRef<str>) -> Result<Self> {
        let trimmed = raw.as_ref().trim();
        if trimmed.len() != 32 || !trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(SpottyError::Io(format!(
                "invalid iconCode (expected 32 hex chars): {trimmed}"
            )));
        }
        Ok(Self(trimmed.to_ascii_lowercase()))
    }

    /// Returns `true` if this icon code matches [`DEFAULT_ICON_HASH`].
    #[must_use]
    pub fn is_default_icon(&self) -> bool {
        self.0 == DEFAULT_ICON_HASH
    }

    /// Returns the normalized 32-character lowercase hex string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for IconCode {
    fn default() -> Self {
        Self(String::from(DEFAULT_ICON_HASH))
    }
}

impl fmt::Display for IconCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for IconCode {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl FromStr for IconCode {
    type Err = SpottyError;

    fn from_str(raw: &str) -> Result<Self> {
        Self::new(raw)
    }
}

impl<'de> Deserialize<'de> for IconCode {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::new(raw).map_err(serde::de::Error::custom)
    }
}

/// Metadata parsed from the LMS plugin `install.xml` manifest.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginManifest {
    /// LMS plugin GUID (`<extension><id>...</id>`).
    pub id: String,
    /// Plugin semantic version (`<version>`).
    pub version: String,
    /// Minimum compatible LMS version (`<targetApplication><minVersion>`).
    pub min_lms_version: String,
    /// Relative settings page URL (`<optionsURL>`).
    pub options_url: String,
    /// Default Spotify client `IconCode` extracted from `<icon>`.
    pub icon_code: IconCode,
}

impl PluginManifest {
    /// Parses an LMS `install.xml` manifest string into a [`PluginManifest`].
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Io`] if any required XML tag is missing, empty,
    /// or contains an invalid `<icon>` filename hash.
    pub fn from_install_xml(xml: &str) -> Result<Self> {
        let cleaned = strip_xml_comments(xml);
        let without_target = remove_element_block(&cleaned, "targetApplication");
        let target_block = extract_tag_inner(&cleaned, "targetApplication").unwrap_or("");

        let id = require_tag(&without_target, "id")?;
        let version = require_tag(&without_target, "version")?;
        let min_lms_version = require_tag(target_block, "minVersion")?;
        let options_url = require_tag(&without_target, "optionsURL")?;
        let icon_path = require_tag(&without_target, "icon")?;
        let icon_hash = extract_icon_hash(icon_path)?;
        let icon_code = IconCode::new(icon_hash)?;

        Ok(Self {
            id: id.to_owned(),
            version: version.to_owned(),
            min_lms_version: min_lms_version.to_owned(),
            options_url: options_url.to_owned(),
            icon_code,
        })
    }
}

fn require_tag<'a>(xml: &'a str, tag: &str) -> Result<&'a str> {
    let Some(value) = extract_tag_inner(xml, tag) else {
        return Err(SpottyError::Io(format!(
            "install.xml missing required <{tag}> element"
        )));
    };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(SpottyError::Io(format!(
            "install.xml has empty <{tag}> element"
        )));
    }
    Ok(trimmed)
}

fn find_open_tag(xml: &str, tag: &str) -> Option<(usize, usize, bool)> {
    let prefix = format!("<{tag}");
    let mut offset = 0;
    while let Some(rel) = xml.get(offset..)?.find(&prefix) {
        let start_idx = offset + rel;
        let after_name = start_idx + prefix.len();
        let next_byte = xml.as_bytes().get(after_name).copied()?;
        if next_byte == b'>' {
            return Some((start_idx, after_name + 1, false));
        }
        if next_byte.is_ascii_whitespace() {
            let mut quote: Option<u8> = None;
            let bytes = xml.as_bytes();
            let mut idx = after_name;
            while idx < bytes.len() {
                let b = bytes[idx];
                if let Some(q) = quote {
                    if b == q {
                        quote = None;
                    }
                } else if b == b'"' || b == b'\'' {
                    quote = Some(b);
                } else if b == b'>' {
                    let self_closing = idx > after_name && bytes[idx - 1] == b'/';
                    return Some((start_idx, idx + 1, self_closing));
                }
                idx += 1;
            }
            return None;
        }
        offset = after_name;
    }
    None
}

fn extract_tag_inner<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let (_start_idx, content_start, self_closing) = find_open_tag(xml, tag)?;
    if self_closing {
        return Some("");
    }
    let close = format!("</{tag}>");
    let rest = xml.get(content_start..)?;
    let end_rel = rest.find(&close)?;
    rest.get(..end_rel)
}

fn remove_element_block(xml: &str, tag: &str) -> String {
    let Some((start_idx, content_start, self_closing)) = find_open_tag(xml, tag) else {
        return xml.to_owned();
    };
    let end_idx = if self_closing {
        content_start
    } else {
        let close = format!("</{tag}>");
        let Some(rest) = xml.get(content_start..) else {
            return xml.to_owned();
        };
        let Some(end_rel) = rest.find(&close) else {
            return xml.to_owned();
        };
        content_start + end_rel + close.len()
    };
    let mut out = String::with_capacity(xml.len());
    if let Some(before) = xml.get(..start_idx) {
        out.push_str(before);
    }
    if let Some(after) = xml.get(end_idx..) {
        out.push_str(after);
    }
    out
}

fn strip_xml_comments(xml: &str) -> String {
    let mut out = String::with_capacity(xml.len());
    let mut cursor = xml;
    while let Some(start) = cursor.find("<!--") {
        if let Some(before) = cursor.get(..start) {
            out.push_str(before);
        }
        let Some(after_open) = cursor.get(start + 4..) else {
            if let Some(tail) = cursor.get(start..) {
                out.push_str(tail);
            }
            return out;
        };
        let Some(end) = after_open.find("-->") else {
            if let Some(tail) = cursor.get(start..) {
                out.push_str(tail);
            }
            return out;
        };
        cursor = after_open.get(end + 3..).unwrap_or("");
    }
    out.push_str(cursor);
    out
}

fn extract_icon_hash(icon_path: &str) -> Result<&str> {
    let file_name = icon_path.rsplit('/').next().unwrap_or(icon_path);
    let stem = file_name.split('.').next().unwrap_or("");
    if stem.is_empty() {
        return Err(SpottyError::Io(format!(
            "install.xml <icon> path missing filename stem: {icon_path}"
        )));
    }
    Ok(stem)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use proptest::{prop_assert, prop_assert_eq, proptest};

    use super::*;

    #[test]
    fn parses_real_install_xml() {
        let raw_xml = include_str!("../../../install.xml");
        let manifest =
            PluginManifest::from_install_xml(raw_xml).expect("root install.xml should parse");

        assert_eq!(manifest.id, "21cbb80e-67b8-44a8-a662-21c6c7ae5260");
        assert_eq!(manifest.version, "4.62.3");
        assert_eq!(manifest.min_lms_version, "7.7");
        assert_eq!(manifest.options_url, "plugins/Spotty/settings/basic.html");
        assert_eq!(manifest.icon_code.as_str(), DEFAULT_ICON_HASH);
        assert!(
            manifest.icon_code.is_default_icon(),
            "expected default icon hash from install.xml"
        );
    }

    #[test]
    fn icon_code_normalizes_uppercase_and_rejects_invalid() {
        let upper = DEFAULT_ICON_HASH.to_ascii_uppercase();
        let code = IconCode::new(&upper).expect("uppercase 32-hex icon code should parse");
        assert_eq!(code.as_str(), DEFAULT_ICON_HASH);
        assert!(code.is_default_icon());

        let custom: IconCode = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
            .parse()
            .expect("valid custom hex should parse");
        assert_eq!(custom.as_str(), "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert!(!custom.is_default_icon());

        for bad in ["", "1234", "93aac68fb06348598c1e67734dfaceeg", "zzzz"] {
            assert!(
                IconCode::new(bad).is_err(),
                "expected IconCode validation failure for {bad:?}"
            );
        }
    }

    #[test]
    fn from_install_xml_rejects_missing_or_corrupt_fields() {
        let missing_version = "<extension><id>abc</id><optionsURL>opt</optionsURL>\
            <icon>a/93aac68fb06348598c1e67734dfaceee.png</icon>\
            <targetApplication><minVersion>7.7</minVersion></targetApplication></extension>";
        assert!(PluginManifest::from_install_xml(missing_version).is_err());

        let bad_icon = "<extension><id>abc</id><version>1.0</version><optionsURL>opt</optionsURL>\
            <icon>a/not-hex.png</icon>\
            <targetApplication><minVersion>7.7</minVersion></targetApplication></extension>";
        assert!(PluginManifest::from_install_xml(bad_icon).is_err());
    }

    #[test]
    fn parses_xml_with_tag_attributes_and_unclosed_comment() {
        let xml = r#"
<extension name="Spotty">
  <id scope="lms">21cbb80e-67b8-44a8-a662-21c6c7ae5260</id>
  <version build="1">4.62.3</version>
  <optionsURL kind="html">plugins/Spotty/settings/basic.html</optionsURL>
  <icon type="png">HTML/EN/plugins/Spotty/html/images/93aac68fb06348598c1e67734dfaceee.png</icon>
  <targetApplication min="true">
    <minVersion operator="ge">7.7</minVersion>
  </targetApplication>
  <!-- unclosed comment at end
</extension>
"#;
        let manifest =
            PluginManifest::from_install_xml(xml).expect("XML with attributes should parse");
        assert_eq!(manifest.id, "21cbb80e-67b8-44a8-a662-21c6c7ae5260");
        assert_eq!(manifest.version, "4.62.3");
        assert_eq!(manifest.min_lms_version, "7.7");
        assert_eq!(manifest.options_url, "plugins/Spotty/settings/basic.html");
        assert_eq!(manifest.icon_code.as_str(), DEFAULT_ICON_HASH);

        let unclosed_before = "\
<extension>\
  <!-- unclosed comment does not drop rest \
  <id>21cbb80e-67b8-44a8-a662-21c6c7ae5260</id>\
  <version>4.62.3</version>\
  <optionsURL>plugins/Spotty/settings/basic.html</optionsURL>\
  <icon>93aac68fb06348598c1e67734dfaceee.png</icon>\
  <targetApplication><minVersion>7.7</minVersion></targetApplication>\
</extension>";
        let parsed = PluginManifest::from_install_xml(unclosed_before)
            .expect("unclosed comment must not truncate remaining XML");
        assert_eq!(parsed.id, "21cbb80e-67b8-44a8-a662-21c6c7ae5260");
    }

    proptest! {
        #[test]
        fn icon_code_accepts_any_32_hex_string_and_lowercases(hex in "[0-9a-fA-F]{32}") {
            let parsed = IconCode::new(&hex).expect("32 hex chars must always validate");
            prop_assert_eq!(parsed.as_str(), hex.to_ascii_lowercase());
            prop_assert_eq!(parsed.as_str().len(), 32);
        }

        #[test]
        fn icon_code_rejects_wrong_length_strings(
            s in "[0-9a-fA-F]{0,31}|[0-9a-fA-F]{33,64}"
        ) {
            prop_assert!(IconCode::new(&s).is_err());
        }
    }
}
