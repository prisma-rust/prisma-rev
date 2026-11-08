//! `prism-editor-tool` — Editor tools for PrismaRev.
//!
//! Provides procedural terrain generation (noise, heightmap, erosion) and
//! image export.  Designed for the PrismaRev editor and asset pipeline but
//! usable standalone.

pub mod erosion;
pub mod export;
pub mod heightmap;
pub mod noise;
