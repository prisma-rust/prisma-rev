//! `assets/settings.ini` document model for the editor tools and the MCP
//! server.
//!
//! These tools only edit a few sections and must write back everything else
//! unchanged, which is why the model keeps section and key order. The dialect
//! and the parser live in [`prism_ini`], shared with the engine and the editor
//! data layer.

pub use prism_ini::{format_value, parse_value, Ini, IniDocument, IniSection};
