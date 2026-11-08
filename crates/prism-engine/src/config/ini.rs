//! Project settings (`assets/settings.ini`) for the engine.
//!
//! The dialect and the parser live in [`prism_ini`]; this module re-exports
//! them so `crate::config::ini` stays the engine-facing path. The engine, the
//! editor tools (`prism-editor-api`) and the editor data layer
//! (`prism-editor-core`) share that one implementation instead of keeping a
//! copy each.

pub use prism_ini::{format_value, parse_value, Ini, IniDocument, IniSection};
