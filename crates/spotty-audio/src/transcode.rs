//! LMS `custom-convert.conf` pipeline and transcoding table command builder.
//!
//! Mirrors `Plugins::Spotty::Plugin::updateTranscodingTable`
//! (`Plugin.pm:236-289`), the `FileTypes` web UI `TMPDIR=` prefix stripper
//! (`Plugin.pm:184-203`), and `custom-convert.conf`.

use std::fmt::Write as _;
use std::path::Path;

use spotty_types::ApFallbackPolicy;
use spotty_types::Bitrate;

use crate::replaygain::NormalisationMode;

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
            let _ = write!(cmd, "TMPDIR={} ", tmp.display());
        }

        let helper_token = format_bracketed_token(helper_bin);
        let _ = write!(
            cmd,
            "{helper_token} -n Squeezebox -c \"{}\" --single-track $URL$ \
             --bitrate {} --disable-discovery",
            cache_dir.display(),
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

    /// Strips any leading `TMPDIR=... ` prefix before the first `[` token so
    /// LMS `Settings/File Types` recognizes the transcoding entry
    /// (`Plugin.pm:184-203`).
    #[must_use]
    pub fn strip_tmpdir_prefix_for_webui(command_line: &str) -> String {
        match command_line.find('[') {
            Some(idx) => command_line[idx..].to_owned(),
            None => command_line.to_owned(),
        }
    }
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
            "TMPDIR=/var/tmp [spotty] -n Squeezebox -c \"/var/cache/spotty/deadbeef\" \
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
            "[spotty-armhf] -n Squeezebox -c \"/cache\" --single-track $URL$ \
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
}
