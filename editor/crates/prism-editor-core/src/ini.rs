//! `assets/settings.ini` document model for the editor data layer.
//!
//! Writing back must keep the sections this layer does not manage, so section
//! and key order are preserved. The dialect and the parser live in
//! [`prism_ini`], shared with the engine and the editor tools.

pub use prism_ini::{format_value, parse_value, Ini, IniDocument, IniSection};
