//! PrismaRev audio subsystem.
//!
//! Built on [Firewheel](https://github.com/BillyDM/firewheel), a pure-Rust
//! audio graph engine, with cpal as the backend. Supports Windows, macOS,
//! Linux, Android, iOS and WebAssembly.
//!
//! # Quick start
//!
//! ```rust,no_run
//! use prism_audio::*;
//!
//! let mut engine = AudioEngine::new(AudioConfig::default())
//!     .expect("audio engine");
//!
//! // Load a sound.
//! let data = decoder::decode_file("beep.wav").unwrap();
//!
//! // Play it.
//! let handle = engine.play(&data);
//!
//! // Adjust the volume; the engine owns the handle.
//! engine.set_volume(&handle, 0.5);
//!
//! // Call once per frame.
//! engine.update();
//! ```

pub mod decoder;
pub mod engine;
pub mod error;

pub use engine::{AudioConfig, AudioData, AudioEngine, PlaybackHandle};
pub use error::AudioError;
