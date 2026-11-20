//! In-process `librespot` track streamer producing raw 44.1 kHz 16-bit signed
//! little-endian stereo PCM bytes (`s16le`).

use std::ffi::OsStr;
use std::fs;
use std::future::Future;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;

use librespot_core::authentication::Credentials;
use librespot_core::cache::Cache;
use librespot_core::config::SessionConfig;
use librespot_core::session::Session;
use librespot_playback::audio_backend::Sink;
use librespot_playback::audio_backend::SinkError;
use librespot_playback::audio_backend::SinkResult;
use librespot_playback::config::Bitrate as LibrespotBitrate;
use librespot_playback::config::PlayerConfig;
use librespot_playback::convert::Converter;
use librespot_playback::decoder::AudioPacket;
use librespot_playback::mixer::NoOpVolume;
use librespot_playback::player::Player;
use librespot_playback::player::PlayerEvent;
use spotty_cache::AccountStore;
use spotty_cache::CredentialsRecord;
use spotty_types::ApFallbackPolicy;
use spotty_types::Bitrate;
use spotty_types::Result;
use spotty_types::SpotifyUri;
use spotty_types::SpottyError;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt as _;
use tokio::sync::mpsc;

use crate::replaygain::NormalisationMode;
use crate::transcode::AudioCachePolicy;

/// Output PCM sample rate in Hz (`44_100`).
pub const PCM_SAMPLE_RATE_HZ: u32 = 44_100;

/// Output PCM channel count (`2` = stereo).
pub const PCM_CHANNELS: u8 = 2;

/// Output PCM sample width in bits (`16` = signed 16-bit little-endian).
pub const PCM_BITS_PER_SAMPLE: u8 = 16;

const PCM_CHANNEL_CAPACITY: usize = 64;
const CREDENTIALS_FILE: &str = "credentials.json";

/// Parameters describing an individual Spotify track or episode stream request.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamRequest {
    /// Directory containing Spotty account credentials and optional audio cache.
    pub cache_dir: PathBuf,
    /// Target Spotify track or episode URI.
    pub uri: SpotifyUri,
    /// Optional start seek offset in whole seconds (`--start-position`).
    pub start_offset_secs: Option<u64>,
    /// Target Spotify Vorbis stream bitrate.
    pub bitrate: Bitrate,
    /// Whether `librespot` may persist decrypted audio chunks to disk.
    pub audio_cache: AudioCachePolicy,
    /// Client-side `librespot` volume normalisation toggle.
    pub normalisation: NormalisationMode,
    /// Access point fallback port policy (`--ap-port=12321`).
    pub ap_fallback: ApFallbackPolicy,
    /// Device display name announced during Spotify session setup.
    pub device_name: String,
}

impl StreamRequest {
    /// Creates a [`StreamRequest`] with default playback settings for `uri`.
    #[must_use]
    pub fn new(cache_dir: impl Into<PathBuf>, uri: SpotifyUri) -> Self {
        Self {
            cache_dir: cache_dir.into(),
            uri,
            start_offset_secs: None,
            bitrate: Bitrate::default(),
            audio_cache: AudioCachePolicy::Disabled,
            normalisation: NormalisationMode::Disabled,
            ap_fallback: ApFallbackPolicy::default(),
            device_name: String::from("Squeezebox"),
        }
    }
}

/// Validated, resolved stream parameters passed to an [`AudioBackend`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedStreamParams {
    /// Original cache directory from the [`StreamRequest`].
    pub cache_dir: PathBuf,
    /// Parsed Spotify URI in canonical colon notation.
    pub uri: SpotifyUri,
    /// Start seek offset converted to milliseconds for `Player::load`.
    pub start_position_ms: u32,
    /// Target Spotify stream bitrate.
    pub bitrate: Bitrate,
    /// Audio cache policy.
    pub audio_cache: AudioCachePolicy,
    /// Client-side volume normalisation setting.
    pub normalisation: NormalisationMode,
    /// Access point fallback port policy.
    pub ap_fallback: ApFallbackPolicy,
    /// Device display name.
    pub device_name: String,
    /// Reusable credentials loaded from [`AccountStore`].
    pub credentials: CredentialsRecord,
}

/// Boxed `Send` future returned by [`AudioBackend::stream_to_channel`].
pub type BackendStreamFuture<'a> = Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;

/// Abstraction over the audio decoding/streaming backend (`librespot` or test
/// double) that emits raw `44_100 Hz` 16-bit signed little-endian stereo PCM
/// chunks into `pcm_tx`.
pub trait AudioBackend: Send + Sync {
    /// Streams raw PCM `s16le` frames for `params` into `pcm_tx` until EOF or
    /// error.
    fn stream_to_channel<'a>(
        &'a self,
        params: ResolvedStreamParams,
        pcm_tx: mpsc::Sender<Vec<u8>>,
    ) -> BackendStreamFuture<'a>;
}

/// Production [`AudioBackend`] backed by in-process `librespot-core` and
/// `librespot-playback`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default)]
pub struct LibrespotBackend;

impl LibrespotBackend {
    /// Creates a new [`LibrespotBackend`].
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl AudioBackend for LibrespotBackend {
    fn stream_to_channel<'a>(
        &'a self,
        params: ResolvedStreamParams,
        pcm_tx: mpsc::Sender<Vec<u8>>,
    ) -> BackendStreamFuture<'a> {
        Box::pin(stream_via_librespot(params, pcm_tx))
    }
}

/// High-level track streamer coordinating credential resolution, seek offset
/// conversion, and async PCM delivery to an [`AsyncWrite`] sink.
#[derive(Debug)]
pub struct TrackStreamer<B> {
    backend: B,
}

impl TrackStreamer<LibrespotBackend> {
    /// Creates a [`TrackStreamer`] using the in-process [`LibrespotBackend`].
    #[must_use]
    pub fn with_librespot() -> Self {
        Self::new(LibrespotBackend::new())
    }
}

impl Default for TrackStreamer<LibrespotBackend> {
    fn default() -> Self {
        Self::with_librespot()
    }
}

impl<B: AudioBackend> TrackStreamer<B> {
    /// Creates a [`TrackStreamer`] backed by a custom [`AudioBackend`].
    #[must_use]
    pub fn new(backend: B) -> Self {
        Self { backend }
    }

    /// Returns a shared reference to the underlying [`AudioBackend`].
    #[must_use]
    pub fn backend(&self) -> &B {
        &self.backend
    }

    /// Streams raw `44_100 Hz`, 16-bit signed little-endian, 2-channel PCM
    /// bytes for `request` into `writer`, returning total bytes written.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::NoAccessToken`] if no credentials exist in
    /// `request.cache_dir`, [`SpottyError::Audio`] if `start_offset_secs`
    /// overflows `u32` milliseconds or `librespot` playback fails, or
    /// [`SpottyError::Io`] if writing to `writer` fails.
    pub async fn stream_pcm_s16le<W>(&self, request: &StreamRequest, writer: &mut W) -> Result<u64>
    where
        W: AsyncWrite + Unpin + Send,
    {
        let start_position_ms = offset_secs_to_ms(request.start_offset_secs)?;
        let credentials = load_credentials_record(&request.cache_dir)?;

        let params = ResolvedStreamParams {
            cache_dir: request.cache_dir.clone(),
            uri: request.uri.clone(),
            start_position_ms,
            bitrate: request.bitrate,
            audio_cache: request.audio_cache,
            normalisation: request.normalisation,
            ap_fallback: request.ap_fallback,
            device_name: request.device_name.clone(),
            credentials,
        };

        let (pcm_tx, mut pcm_rx) = mpsc::channel::<Vec<u8>>(PCM_CHANNEL_CAPACITY);
        let backend_fut = self.backend.stream_to_channel(params, pcm_tx);
        let write_fut = async {
            let mut total_bytes = 0u64;
            while let Some(chunk) = pcm_rx.recv().await {
                if chunk.is_empty() {
                    continue;
                }
                writer.write_all(&chunk).await?;
                total_bytes = total_bytes.saturating_add(chunk.len() as u64);
            }
            writer.flush().await?;
            Ok::<u64, SpottyError>(total_bytes)
        };

        let ((), bytes_written) = tokio::try_join!(backend_fut, write_fut)?;
        Ok(bytes_written)
    }
}

/// Converts an optional start seek offset in seconds into `librespot`
/// `u32` milliseconds.
///
/// # Errors
///
/// Returns [`SpottyError::Audio`] if `secs * 1000` exceeds `u32::MAX`.
pub fn offset_secs_to_ms(start_offset_secs: Option<u64>) -> Result<u32> {
    let Some(secs) = start_offset_secs else {
        return Ok(0);
    };
    let ms = secs.checked_mul(1_000).ok_or_else(|| {
        SpottyError::Audio(format!(
            "seek offset {secs}s overflows millisecond conversion"
        ))
    })?;
    u32::try_from(ms).map_err(|_| {
        SpottyError::Audio(format!(
            "seek offset {secs}s exceeds 32-bit millisecond limit"
        ))
    })
}

/// Encodes interleaved signed 16-bit PCM samples as raw little-endian bytes
/// (`s16le`).
#[must_use]
pub fn encode_s16le_samples(samples: &[i16]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(samples.len().saturating_mul(2));
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    bytes
}

/// Loads reusable Spotify credentials from `cache_dir` using [`AccountStore`].
///
/// Supports direct account folders containing `credentials.json`, base cache
/// directories (`<cache>/spotty/<account>/credentials.json`), and
/// `<cache>/spotty` directories.
///
/// # Errors
///
/// Returns [`SpottyError::NoAccessToken`] if no credentials can be located, or
/// [`SpottyError::Io`] / [`SpottyError::Json`] if reading fails.
pub fn load_credentials_record(cache_dir: &Path) -> Result<CredentialsRecord> {
    let direct = cache_dir.join(CREDENTIALS_FILE);
    if direct.is_file() {
        let raw = fs::read_to_string(&direct)?;
        let record: CredentialsRecord = serde_json::from_str(&raw)?;
        return validate_credentials_record(record);
    }

    if let Some(record) = try_load_from_account_store(&AccountStore::new(cache_dir))? {
        return Ok(record);
    }

    if cache_dir.file_name() == Some(OsStr::new("spotty")) {
        if let Some(parent) = cache_dir.parent() {
            if let Some(record) = try_load_from_account_store(&AccountStore::new(parent))? {
                return Ok(record);
            }
        }
    }

    Err(SpottyError::NoAccessToken)
}

fn try_load_from_account_store(store: &AccountStore) -> Result<Option<CredentialsRecord>> {
    if let Some(player_creds) = store.player_credentials()? {
        if has_reusable_auth(&player_creds) {
            return Ok(Some(player_creds));
        }
    }

    let accounts = store.all_credentials()?;
    for (_username, account_id) in accounts {
        if let Some(record) = store.credentials(&account_id)? {
            if has_reusable_auth(&record) {
                return Ok(Some(record));
            }
        }
    }

    Ok(None)
}

fn has_reusable_auth(record: &CredentialsRecord) -> bool {
    !record.username.is_empty()
        && record.auth_type.is_some()
        && record.auth_data.as_deref().is_some_and(|s| !s.is_empty())
}

fn validate_credentials_record(record: CredentialsRecord) -> Result<CredentialsRecord> {
    if has_reusable_auth(&record) {
        Ok(record)
    } else {
        Err(SpottyError::NoAccessToken)
    }
}

fn to_librespot_credentials(record: &CredentialsRecord) -> Result<Credentials> {
    let json = serde_json::to_string(record)?;
    let creds: Credentials = serde_json::from_str(&json)?;
    Ok(creds)
}

fn map_librespot_bitrate(bitrate: Bitrate) -> LibrespotBitrate {
    match bitrate {
        Bitrate::Kbps96 => LibrespotBitrate::Bitrate96,
        Bitrate::Kbps160 => LibrespotBitrate::Bitrate160,
        Bitrate::Kbps320 => LibrespotBitrate::Bitrate320,
        _ => LibrespotBitrate::Bitrate320,
    }
}

async fn stream_via_librespot(
    params: ResolvedStreamParams,
    pcm_tx: mpsc::Sender<Vec<u8>>,
) -> Result<()> {
    let credentials = to_librespot_credentials(&params.credentials)?;
    let librespot_uri = librespot_core::SpotifyUri::from_uri(&params.uri.to_colon_uri())
        .map_err(|err| SpottyError::InvalidUri(err.to_string()))?;

    let session_config = SessionConfig {
        ap_port: match params.ap_fallback {
            ApFallbackPolicy::ForcePort12321 => Some(12_321),
            _ => None,
        },
        tmp_dir: params.cache_dir.clone(),
        ..SessionConfig::default()
    };

    let audio_cache_dir =
        (params.audio_cache == AudioCachePolicy::Enabled).then(|| params.cache_dir.join("audio"));
    let cache = Cache::new(
        Some(params.cache_dir.as_path()),
        None::<&Path>,
        audio_cache_dir.as_deref(),
        None,
    )
    .map_err(|err| SpottyError::Cache(err.to_string()))?;

    let session = Session::new(session_config, Some(cache));
    session
        .connect(credentials, false)
        .await
        .map_err(|err| SpottyError::Audio(format!("Spotify session connect failed: {err}")))?;

    let player_config = PlayerConfig {
        bitrate: map_librespot_bitrate(params.bitrate),
        normalisation: params.normalisation == NormalisationMode::Enabled,
        ..PlayerConfig::default()
    };

    let player = Player::new(player_config, session, Box::new(NoOpVolume), move || {
        Box::new(PcmChannelSink::new(pcm_tx))
    });

    let mut events = player.get_player_event_channel();
    player.load(librespot_uri, true, params.start_position_ms);

    let mut outcome = Ok(());
    while let Some(event) = events.recv().await {
        match event {
            PlayerEvent::EndOfTrack { .. } | PlayerEvent::Stopped { .. } => break,
            PlayerEvent::Unavailable { track_id, .. } => {
                outcome = Err(SpottyError::Audio(format!(
                    "Spotify track unavailable: {track_id}"
                )));
                break;
            }
            _ => {}
        }
    }

    player.stop();
    drop(player);
    outcome
}

/// Custom `librespot_playback` [`Sink`] forwarding decoded `44_100 Hz` 16-bit
/// signed little-endian stereo PCM frames into a bounded `tokio` channel.
#[derive(Debug)]
pub struct PcmChannelSink {
    tx: Option<mpsc::Sender<Vec<u8>>>,
}

impl PcmChannelSink {
    /// Creates a new [`PcmChannelSink`] targeting `tx`.
    #[must_use]
    pub fn new(tx: mpsc::Sender<Vec<u8>>) -> Self {
        Self { tx: Some(tx) }
    }

    fn send_bytes(&self, bytes: Vec<u8>) -> SinkResult<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        let Some(tx) = self.tx.as_ref() else {
            return Err(SinkError::NotConnected(String::from(
                "PCM sink channel already closed",
            )));
        };
        tx.blocking_send(bytes)
            .map_err(|err| SinkError::NotConnected(format!("PCM receiver dropped: {err}")))
    }
}

impl Sink for PcmChannelSink {
    fn start(&mut self) -> SinkResult<()> {
        Ok(())
    }

    fn stop(&mut self) -> SinkResult<()> {
        self.tx = None;
        Ok(())
    }

    fn write(&mut self, packet: AudioPacket, converter: &mut Converter) -> SinkResult<()> {
        match packet {
            AudioPacket::Samples(samples) => {
                let pcm_i16 = converter.f64_to_s16(&samples);
                let bytes = encode_s16le_samples(&pcm_i16);
                self.send_bytes(bytes)
            }
            AudioPacket::Raw(raw) => self.send_bytes(raw),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;

    use spotty_cache::AccountId;
    use tempfile::TempDir;

    use super::*;

    #[derive(Debug)]
    struct MockAudioBackend {
        captured_params: Arc<Mutex<Option<ResolvedStreamParams>>>,
        samples_to_emit: Vec<i16>,
    }

    impl MockAudioBackend {
        fn new(samples_to_emit: Vec<i16>) -> Self {
            Self {
                captured_params: Arc::new(Mutex::new(None)),
                samples_to_emit,
            }
        }
    }

    impl AudioBackend for MockAudioBackend {
        fn stream_to_channel<'a>(
            &'a self,
            params: ResolvedStreamParams,
            pcm_tx: mpsc::Sender<Vec<u8>>,
        ) -> BackendStreamFuture<'a> {
            let captured = Arc::clone(&self.captured_params);
            let samples = self.samples_to_emit.clone();
            Box::pin(async move {
                *captured
                    .lock()
                    .expect("captured params mutex is never poisoned") = Some(params);
                let bytes = encode_s16le_samples(&samples);
                pcm_tx
                    .send(bytes)
                    .await
                    .map_err(|err| SpottyError::Audio(err.to_string()))?;
                Ok(())
            })
        }
    }

    fn seed_account_credentials(base_dir: &Path, username: &str) -> AccountId {
        let store = AccountStore::new(base_dir);
        let account_id = AccountId::from_username(username);
        let account_dir = store.account_dir(&account_id);
        fs::create_dir_all(&account_dir).expect("account dir should be created");
        // Base64 of b"secret-token-blob"
        let creds_json = format!(
            "{{\"username\":\"{username}\",\"auth_type\":1,\
             \"auth_data\":\"c2VjcmV0LXRva2VuLWJsb2I=\"}}"
        );
        fs::write(account_dir.join(CREDENTIALS_FILE), creds_json)
            .expect("credentials.json should be written");
        account_id
    }

    #[test]
    fn pcm_s16le_encoding_writes_little_endian_signed_samples() {
        let samples = [0_i16, 1_i16, -1_i16, i16::MAX, i16::MIN];
        let encoded = encode_s16le_samples(&samples);
        assert_eq!(
            encoded,
            vec![
                0x00, 0x00, // 0
                0x01, 0x00, // 1
                0xFF, 0xFF, // -1
                0xFF, 0x7F, // 32767
                0x00, 0x80, // -32768
            ],
            "s16le bytes must match two's complement little-endian layout"
        );
        assert_eq!(PCM_SAMPLE_RATE_HZ, 44_100);
        assert_eq!(PCM_CHANNELS, 2);
        assert_eq!(PCM_BITS_PER_SAMPLE, 16);
    }

    #[test]
    fn offset_secs_to_ms_handles_none_valid_and_overflow() {
        assert_eq!(
            offset_secs_to_ms(None).expect("None offset should map to 0"),
            0
        );
        assert_eq!(
            offset_secs_to_ms(Some(42)).expect("42s should convert to 42_000 ms"),
            42_000
        );
        let err = offset_secs_to_ms(Some(u64::from(u32::MAX)))
            .expect_err("overflowing millisecond conversion must fail");
        assert!(
            matches!(err, SpottyError::Audio(_)),
            "expected Audio error on seek overflow, got {err:?}"
        );
    }

    #[test]
    fn pcm_channel_sink_converts_f64_packets_to_s16le_bytes() {
        let (tx, mut rx) = mpsc::channel(4);
        let mut sink = PcmChannelSink::new(tx);
        let mut converter = Converter::new(None);
        sink.start().expect("sink start should succeed");
        sink.write(
            AudioPacket::Samples(vec![0.0_f64, 0.5_f64, -0.5_f64]),
            &mut converter,
        )
        .expect("sink write should succeed");
        sink.stop().expect("sink stop should succeed");

        let chunk = rx
            .blocking_recv()
            .expect("PCM chunk should be received on channel");
        assert_eq!(
            chunk.len(),
            6,
            "3 stereo/mono samples at s16le must yield 6 bytes"
        );
        let s0 = i16::from_le_bytes([chunk[0], chunk[1]]);
        let s1 = i16::from_le_bytes([chunk[2], chunk[3]]);
        let s2 = i16::from_le_bytes([chunk[4], chunk[5]]);
        assert_eq!(s0, 0);
        assert!(s1 > 15_000 && s1 < 17_000, "unexpected +0.5 sample: {s1}");
        assert!(s2 < -15_000 && s2 > -17_000, "unexpected -0.5 sample: {s2}");
    }

    #[tokio::test]
    async fn track_streamer_loads_account_store_creds_and_streams_s16le() {
        let tmp = TempDir::new().expect("temp dir should be created");
        seed_account_credentials(tmp.path(), "spotty_tester");

        let uri: SpotifyUri = "spotify:track:4uLU6hMCjMI75M1A2tKUQC"
            .parse()
            .expect("track URI should parse");
        let mut req = StreamRequest::new(tmp.path(), uri.clone());
        req.start_offset_secs = Some(15);
        req.bitrate = Bitrate::Kbps160;
        req.normalisation = NormalisationMode::Enabled;
        req.ap_fallback = ApFallbackPolicy::ForcePort12321;

        let mock = MockAudioBackend::new(vec![100, -200, 32_000, -32_000]);
        let captured = Arc::clone(&mock.captured_params);
        let streamer = TrackStreamer::new(mock);

        let mut pcm_out = Vec::new();
        let written = streamer
            .stream_pcm_s16le(&req, &mut pcm_out)
            .await
            .expect("mock stream_pcm_s16le should succeed");

        assert_eq!(written, 8);
        assert_eq!(pcm_out, encode_s16le_samples(&[100, -200, 32_000, -32_000]));

        let params = captured
            .lock()
            .expect("captured mutex is never poisoned")
            .clone()
            .expect("backend should have been invoked");
        assert_eq!(params.uri, uri);
        assert_eq!(params.start_position_ms, 15_000);
        assert_eq!(params.bitrate, Bitrate::Kbps160);
        assert_eq!(params.normalisation, NormalisationMode::Enabled);
        assert_eq!(params.ap_fallback, ApFallbackPolicy::ForcePort12321);
        assert_eq!(params.credentials.username, "spotty_tester");

        let librespot_creds = to_librespot_credentials(&params.credentials)
            .expect("CredentialsRecord should convert to librespot Credentials");
        assert_eq!(librespot_creds.username.as_deref(), Some("spotty_tester"));
        assert_eq!(librespot_creds.auth_data, b"secret-token-blob");
    }

    #[tokio::test]
    async fn track_streamer_returns_no_access_token_when_cache_is_empty() {
        let tmp = TempDir::new().expect("temp dir should be created");
        let uri: SpotifyUri = "spotify:track:4uLU6hMCjMI75M1A2tKUQC"
            .parse()
            .expect("track URI should parse");
        let req = StreamRequest::new(tmp.path(), uri);
        let streamer = TrackStreamer::new(MockAudioBackend::new(vec![1, 2]));
        let mut out = Vec::new();
        let err = streamer
            .stream_pcm_s16le(&req, &mut out)
            .await
            .expect_err("missing credentials must fail");
        assert_eq!(err, SpottyError::NoAccessToken);
    }
}
