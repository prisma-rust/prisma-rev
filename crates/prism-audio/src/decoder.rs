//! Audio file decoding.
//!
//! Decodes common audio formats into raw f32 samples for playback,
//! using the high-level `symphonium` API.

use std::path::Path;
use std::time::Duration;

use symphonium::{DecodeConfig, DecodedAudioF32};

use crate::error::AudioError;
use crate::AudioData;

/// Supported audio formats.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioFormat {
    Wav,
    Ogg,
    Mp3,
    Flac,
}

impl AudioFormat {
    /// Infer format from a file extension.
    pub fn from_extension(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_lowercase();
        match ext.as_str() {
            "wav" => Some(Self::Wav),
            "ogg" => Some(Self::Ogg),
            "mp3" => Some(Self::Mp3),
            "flac" => Some(Self::Flac),
            _ => None,
        }
    }

    fn to_extension_hint(self) -> &'static str {
        match self {
            Self::Wav => "wav",
            Self::Ogg => "ogg",
            Self::Mp3 => "mp3",
            Self::Flac => "flac",
        }
    }
}

/// Decode an audio file from raw bytes.
///
/// Uses auto-detection of the format. Falls back to [`AudioFormat`]
/// if provided as a hint when auto-detection might be ambiguous.
pub fn decode_bytes(bytes: &[u8], format: AudioFormat) -> Result<AudioData, AudioError> {
    let mut hint = symphonia::core::formats::probe::Hint::new();
    hint.with_extension(format.to_extension_hint());

    let cursor = Box::new(std::io::Cursor::new(bytes.to_vec()));
    let probed = symphonium::probe_from_source(cursor, Some(hint), None)
        .map_err(|e| AudioError::Decode(e.to_string()))?;

    decode_probed(probed)
}

/// Decode an audio file, auto-detecting the format from content.
pub fn decode_auto(bytes: &[u8]) -> Result<AudioData, AudioError> {
    let cursor = Box::new(std::io::Cursor::new(bytes.to_vec()));
    let probed = symphonium::probe_from_source(cursor, None, None)
        .map_err(|e| AudioError::Decode(e.to_string()))?;

    decode_probed(probed)
}

/// Decode a file on disk.
pub fn decode_file(path: impl AsRef<Path>) -> Result<AudioData, AudioError> {
    let path = path.as_ref();
    let probed = symphonium::probe_from_file(path, None)
        .map_err(|e| AudioError::Decode(format!("Cannot probe {}: {e}", path.display())))?;

    decode_probed(probed)
}

// ---------------------------------------------------------------------------
// Shared helper: decode a probed source → AudioData

fn decode_probed(probed: symphonium::ProbedAudioSource) -> Result<AudioData, AudioError> {
    let config = DecodeConfig::default();
    let decoded: DecodedAudioF32 = symphonium::decode_f32(probed, &config, None, None, None)
        .map_err(|e| AudioError::Decode(e.to_string()))?;

    if decoded.frames() == 0 {
        return Err(AudioError::Decode("Decoded zero samples".into()));
    }

    let channels = decoded.channels();
    let num_frames = decoded.frames();
    let sample_rate = decoded.sample_rate.get();

    // Convert planar → interleaved (L,R,L,R,...)
    let mut samples = Vec::with_capacity(channels * num_frames);
    for frame in 0..num_frames {
        #[allow(clippy::needless_range_loop)]
        for ch in 0..channels {
            samples.push(decoded.data[ch][frame]);
        }
    }

    let duration = Duration::from_secs_f64(num_frames as f64 / sample_rate as f64);

    Ok(AudioData {
        samples,
        sample_rate,
        channels: channels as u16,
        duration,
    })
}
