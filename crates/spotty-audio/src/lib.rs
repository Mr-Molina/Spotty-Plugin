//! Audio streaming, Connect authentication, and transcode table builder.

pub mod replaygain;
pub mod streamer;
pub mod transcode;

pub use crate::replaygain::{resolve_lms_track_gain, NormalisationMode, ServerReplayGainMode};
pub use crate::streamer::{
    encode_s16le_samples, load_credentials_record, offset_secs_to_ms, AudioBackend,
    BackendStreamFuture, LibrespotBackend, PcmChannelSink, ResolvedStreamParams, StreamRequest,
    TrackStreamer, PCM_BITS_PER_SAMPLE, PCM_CHANNELS, PCM_SAMPLE_RATE_HZ,
};
pub use crate::transcode::{AudioCachePolicy, TranscodeTableBuilder, TranscodeTarget};
