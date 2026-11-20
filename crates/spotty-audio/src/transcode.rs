//! LMS `custom-convert.conf` pipeline and transcoding table command builder.
//!
//! Mirrors `Plugins::Spotty::Plugin::updateTranscodingTable`
//! (`Plugin.pm:236-289`), the `FileTypes` web UI `TMPDIR=` prefix stripper
//! (`Plugin.pm:184-203`), and `custom-convert.conf`.

use std::fmt::Write as _;
use std::path::Path;

use spotty_types::ApFallbackPolicy;
use spotty_types::Bitrate;
use spotty_types::Result;
use spotty_types::SpottyError;

use crate::replaygain::NormalisationMode;

const MAX_LAME_QUALITY: u8 = 9;

/// Controls whether the Spotty helper caches decrypted audio files on disk.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum AudioCachePolicy {
    /// Allow `librespot` to cache audio chunks on disk.
    Enabled,
    /// Pass `--disable-audio-cache` (matching `Plugin.pm:280`).
    #[default]
    Disabled,
}

/// Target LMS output codec pipeline in `custom-convert.conf`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TranscodeTarget {
    /// Uncompressed 44.1 kHz / 16-bit LE stereo PCM (`spt pcm * *`).
    Pcm,
    /// Lossless FLAC re-encoded from PCM (`spt flc * *`).
    Flac,
    /// LAME MP3 transcode (`spt mp3 * *`).
    Mp3 {
        /// LAME `-q` quality level (`0`..=`9`).
        quality: u8,
        /// Optional average bitrate in kbps (`--abr <kbps>`).
        abr_kbps: Option<u32>,
    },
}

/// Stateless builder for LMS `spt-*` transcoding command lines and
/// `custom-convert.conf` templates.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default)]
pub struct TranscodeTableBuilder;

impl TranscodeTableBuilder {
    /// Creates a new [`TranscodeTableBuilder`].
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Builds a `spotty` helper invocation string matching
    /// `Plugin.pm:272-288` and `custom-convert.conf`.
    ///
    /// Emits `-n Squeezebox -c "<cache>" --single-track $URL$`, `--bitrate`,
    /// `--disable-discovery`, optional `--disable-audio-cache`, optional
    /// `--enable-volume-normalisation`, optional `--ap-port=12321`, and `$START$`.
    #[must_use]
    pub fn build_spotty_command(
        helper_bin: &str,
        cache_dir: &Path,
        tmp_dir: Option<&Path>,
        bitrate: Bitrate,
        audio_cache: AudioCachePolicy,
        normalisation: NormalisationMode,
        ap_fallback: ApFallbackPolicy,
    ) -> String {
        let mut cmd = String::new();
        if let Some(tmp) = tmp_dir {
            let quoted_tmp = shell_quote_posix(&tmp.to_string_lossy());
            let _ = write!(cmd, "TMPDIR={quoted_tmp} ");
        }

        let helper_token = format_bracketed_token(helper_bin);
        let quoted_cache = shell_quote_posix(&cache_dir.to_string_lossy());
        let _ = write!(
            cmd,
            "{helper_token} -n Squeezebox -c {quoted_cache} --single-track $URL$ \
             --bitrate {} --disable-discovery",
            bitrate.kbps()
        );

        if audio_cache == AudioCachePolicy::Disabled {
            cmd.push_str(" --disable-audio-cache");
        }
        if normalisation == NormalisationMode::Enabled {
            cmd.push_str(" --enable-volume-normalisation");
        }
        if ap_fallback == ApFallbackPolicy::ForcePort12321 {
            cmd.push_str(" --ap-port=12321");
        }

        cmd.push_str(" $START$");
        cmd
    }

    /// Builds a complete LMS pipeline command for the requested
    /// [`TranscodeTarget`].
    ///
    /// Clamps MP3 `quality` to `0..=9` for backward compatibility; use
    /// [`Self::try_build_pipeline_command`] to reject out-of-range quality.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn build_pipeline_command(
        helper_bin: &str,
        cache_dir: &Path,
        tmp_dir: Option<&Path>,
        bitrate: Bitrate,
        audio_cache: AudioCachePolicy,
        normalisation: NormalisationMode,
        ap_fallback: ApFallbackPolicy,
        target: TranscodeTarget,
    ) -> String {
        let clamped_target = match target {
            TranscodeTarget::Mp3 { quality, abr_kbps } => TranscodeTarget::Mp3 {
                quality: quality.min(MAX_LAME_QUALITY),
                abr_kbps,
            },
            other => other,
        };
        Self::render_pipeline_command(
            helper_bin,
            cache_dir,
            tmp_dir,
            bitrate,
            audio_cache,
            normalisation,
            ap_fallback,
            clamped_target,
        )
    }

    /// Validated variant of [`Self::build_pipeline_command`] that rejects
    /// MP3 `quality > 9`.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Audio`] if `target` is [`TranscodeTarget::Mp3`]
    /// with `quality > 9`.
    #[allow(clippy::too_many_arguments)]
    pub fn try_build_pipeline_command(
        helper_bin: &str,
        cache_dir: &Path,
        tmp_dir: Option<&Path>,
        bitrate: Bitrate,
        audio_cache: AudioCachePolicy,
        normalisation: NormalisationMode,
        ap_fallback: ApFallbackPolicy,
        target: TranscodeTarget,
    ) -> Result<String> {
        if let TranscodeTarget::Mp3 { quality, .. } = target {
            if quality > MAX_LAME_QUALITY {
                return Err(SpottyError::Audio(format!(
                    "invalid MP3 LAME quality {quality}; expected 0..={MAX_LAME_QUALITY}"
                )));
            }
        }
        Ok(Self::render_pipeline_command(
            helper_bin,
            cache_dir,
            tmp_dir,
            bitrate,
            audio_cache,
            normalisation,
            ap_fallback,
            target,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn render_pipeline_command(
        helper_bin: &str,
        cache_dir: &Path,
        tmp_dir: Option<&Path>,
        bitrate: Bitrate,
        audio_cache: AudioCachePolicy,
        normalisation: NormalisationMode,
        ap_fallback: ApFallbackPolicy,
        target: TranscodeTarget,
    ) -> String {
        let source = Self::build_spotty_command(
            helper_bin,
            cache_dir,
            tmp_dir,
            bitrate,
            audio_cache,
            normalisation,
            ap_fallback,
        );
        match target {
            TranscodeTarget::Pcm => source,
            TranscodeTarget::Flac => format!(
                "{source} | [flac] -cs --channels=2 --sample-rate=44100 --bps=16 \
                 --endian=little --sign=signed --fast --totally-silent --ignore-chunk-sizes -"
            ),
            TranscodeTarget::Mp3 { quality, abr_kbps } => {
                if let Some(abr) = abr_kbps {
                    format!("{source} | [lame] -r --silent -q {quality} --abr {abr} - -")
                } else {
                    format!("{source} | [lame] -r --silent -q {quality} - -")
                }
            }
        }
    }

    /// Renders the full LMS `custom-convert.conf` template for `spt` (`pcm`,
    /// `flc`, and `mp3` pipelines).
    #[must_use]
    pub fn build_custom_convert_conf(helper_bin_token: &str) -> String {
        let token = format_bracketed_token(helper_bin_token);
        format!(
            "spt pcm * *\n\
             \t# RT:{{START=--start-position %s}}\n\
             \t{token} -n Squeezebox -c \"$CACHE$\" --single-track $URL$ --bitrate 320 \
             --disable-discovery --disable-audio-cache $START$\n\
             \n\
             spt flc * *\n\
             \t# RT:{{START=--start-position %s}}\n\
             \t{token} -n Squeezebox -c \"$CACHE$\" --single-track $URL$ --bitrate 320 \
             --disable-discovery --disable-audio-cache $START$ | [flac] -cs --channels=2 \
             --sample-rate=44100 --bps=16 --endian=little --sign=signed --fast \
             --totally-silent --ignore-chunk-sizes -\n\
             \n\
             spt mp3 * *\n\
             \t# RB:{{BITRATE=--abr %B}}T:{{START=--start-position %s}}\n\
             \t{token} -n Squeezebox -c \"$CACHE$\" --single-track $URL$ --bitrate 320 \
             --disable-discovery --disable-audio-cache $START$ | [lame] -r --silent \
             -q $QUALITY$ $BITRATE$ - -\n"
        )
    }

    /// Strips any leading `TMPDIR=... ` environment assignment prefix so LMS
    /// `Settings/File Types` recognizes the transcoding entry
    /// (`Plugin.pm:184-203`), even if the `TMPDIR` path contains `[` or spaces.
    #[must_use]
    pub fn strip_tmpdir_prefix_for_webui(command_line: &str) -> String {
        let trimmed = command_line.trim_start();
        let Some(rest) = trimmed.strip_prefix("TMPDIR=") else {
            return command_line.to_owned();
        };

        let after_val = if let Some(mut inner) = rest.strip_prefix('\'') {
            loop {
                if let Some(idx) = inner.find('\'') {
                    let after_q = &inner[idx + 1..];
                    if let Some(escaped) = after_q.strip_prefix("\\''") {
                        inner = escaped;
                        continue;
                    }
                    break after_q;
                }
                return command_line.to_owned();
            }
        } else if let Some(inner) = rest.strip_prefix('"') {
            let mut escaped = false;
            let mut end_idx = None;
            for (idx, ch) in inner.char_indices() {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    end_idx = Some(idx + 1);
                    break;
                }
            }
            match end_idx {
                Some(idx) => &inner[idx..],
                None => return command_line.to_owned(),
            }
        } else {
            match rest.find(char::is_whitespace) {
                Some(idx) => &rest[idx..],
                None => return String::new(),
            }
        };

        after_val.trim_start().to_owned()
    }
}

fn shell_quote_posix(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

fn format_bracketed_token(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::from("[spotty]");
    }
    if trimmed.starts_with('[') && trimmed.ends_with(']') {
        trimmed.to_owned()
    } else {
        format!("[{trimmed}]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_spotty_command_with_all_optional_flags() {
        let cmd = TranscodeTableBuilder::build_spotty_command(
            "spotty",
            Path::new("/var/cache/spotty/deadbeef"),
            Some(Path::new("/var/tmp")),
            Bitrate::Kbps320,
            AudioCachePolicy::Disabled,
            NormalisationMode::Enabled,
            ApFallbackPolicy::ForcePort12321,
        );

        assert_eq!(
            cmd,
            "TMPDIR='/var/tmp' [spotty] -n Squeezebox -c '/var/cache/spotty/deadbeef' \
             --single-track $URL$ --bitrate 320 --disable-discovery \
             --disable-audio-cache --enable-volume-normalisation --ap-port=12321 $START$"
        );
    }

    #[test]
    fn builds_spotty_command_without_optional_flags() {
        let cmd = TranscodeTableBuilder::build_spotty_command(
            "[spotty-armhf]",
            Path::new("/cache"),
            None,
            Bitrate::Kbps160,
            AudioCachePolicy::Enabled,
            NormalisationMode::Disabled,
            ApFallbackPolicy::DefaultPort,
        );

        assert_eq!(
            cmd,
            "[spotty-armhf] -n Squeezebox -c '/cache' --single-track $URL$ \
             --bitrate 160 --disable-discovery $START$"
        );
        assert!(
            !cmd.contains("TMPDIR="),
            "unexpected TMPDIR prefix in {cmd}"
        );
        assert!(
            !cmd.contains("--disable-audio-cache"),
            "unexpected audio cache disable flag in {cmd}"
        );
        assert!(
            !cmd.contains("--enable-volume-normalisation"),
            "unexpected normalisation flag in {cmd}"
        );
        assert!(
            !cmd.contains("--ap-port"),
            "unexpected ap-port flag in {cmd}"
        );
    }

    #[test]
    fn builds_custom_convert_conf_with_all_three_pipelines() {
        let conf = TranscodeTableBuilder::build_custom_convert_conf("spotty-custom");
        assert!(
            conf.contains("spt pcm * *\n\t# RT:{START=--start-position %s}\n\t[spotty-custom] "),
            "missing spt pcm entry:\n{conf}"
        );
        assert!(
            conf.contains("spt flc * *\n\t# RT:{START=--start-position %s}\n\t[spotty-custom] ")
                && conf.contains("| [flac] -cs --channels=2 --sample-rate=44100 --bps=16"),
            "missing spt flc entry:\n{conf}"
        );
        assert!(
            conf.contains(
                "spt mp3 * *\n\t# RB:{BITRATE=--abr %B}\
                 T:{START=--start-position %s}\n\t[spotty-custom] "
            ) && conf.contains("| [lame] -r --silent -q $QUALITY$ $BITRATE$ - -"),
            "missing spt mp3 entry:\n{conf}"
        );
    }

    #[test]
    fn strips_tmpdir_prefix_for_webui_matching_plugin_pm() {
        let with_tmp = "TMPDIR=/mnt/ramdisk [spotty] -n Squeezebox -c \"/cache\" $START$";
        assert_eq!(
            TranscodeTableBuilder::strip_tmpdir_prefix_for_webui(with_tmp),
            "[spotty] -n Squeezebox -c \"/cache\" $START$"
        );

        let without_tmp = "[spotty] -n Squeezebox -c \"/cache\" $START$";
        assert_eq!(
            TranscodeTableBuilder::strip_tmpdir_prefix_for_webui(without_tmp),
            without_tmp
        );
    }

    #[test]
    fn builds_pipeline_commands_for_flac_and_mp3_targets() {
        let flac = TranscodeTableBuilder::build_pipeline_command(
            "spotty",
            Path::new("/cache"),
            None,
            Bitrate::Kbps96,
            AudioCachePolicy::Disabled,
            NormalisationMode::Disabled,
            ApFallbackPolicy::DefaultPort,
            TranscodeTarget::Flac,
        );
        assert!(flac.contains("--bitrate 96"), "expected 96 kbps in {flac}");
        assert!(
            flac.ends_with("--ignore-chunk-sizes -"),
            "expected flac pipeline suffix in {flac}"
        );

        let mp3 = TranscodeTableBuilder::build_pipeline_command(
            "spotty",
            Path::new("/cache"),
            None,
            Bitrate::Kbps320,
            AudioCachePolicy::Disabled,
            NormalisationMode::Disabled,
            ApFallbackPolicy::DefaultPort,
            TranscodeTarget::Mp3 {
                quality: 2,
                abr_kbps: Some(192),
            },
        );
        assert!(
            mp3.ends_with("| [lame] -r --silent -q 2 --abr 192 - -"),
            "expected lame pipeline suffix in {mp3}"
        );
    }

    #[test]
    fn quotes_paths_with_spaces_quotes_and_brackets_safely() {
        let cmd = TranscodeTableBuilder::build_spotty_command(
            "spotty",
            Path::new("/var/cache/my 'spotty' $dir"),
            Some(Path::new("/tmp/[bracketed] dir")),
            Bitrate::Kbps320,
            AudioCachePolicy::Disabled,
            NormalisationMode::Disabled,
            ApFallbackPolicy::DefaultPort,
        );
        assert!(cmd.starts_with("TMPDIR='/tmp/[bracketed] dir' [spotty] "));
        assert!(cmd.contains("-c '/var/cache/my '\\''spotty'\\'' $dir'"));
        assert_eq!(
            TranscodeTableBuilder::strip_tmpdir_prefix_for_webui(&cmd),
            "[spotty] -n Squeezebox -c '/var/cache/my '\\''spotty'\\'' $dir' \
             --single-track $URL$ --bitrate 320 --disable-discovery \
             --disable-audio-cache $START$"
        );
    }

    #[test]
    fn validates_and_clamps_mp3_lame_quality() {
        let clamped = TranscodeTableBuilder::build_pipeline_command(
            "spotty",
            Path::new("/cache"),
            None,
            Bitrate::Kbps320,
            AudioCachePolicy::Disabled,
            NormalisationMode::Disabled,
            ApFallbackPolicy::DefaultPort,
            TranscodeTarget::Mp3 {
                quality: 42,
                abr_kbps: None,
            },
        );
        assert!(clamped.ends_with("-q 9 - -"));

        let rejected = TranscodeTableBuilder::try_build_pipeline_command(
            "spotty",
            Path::new("/cache"),
            None,
            Bitrate::Kbps320,
            AudioCachePolicy::Disabled,
            NormalisationMode::Disabled,
            ApFallbackPolicy::DefaultPort,
            TranscodeTarget::Mp3 {
                quality: 10,
                abr_kbps: None,
            },
        );
        assert!(matches!(rejected, Err(SpottyError::Audio(_))));
    }
}
