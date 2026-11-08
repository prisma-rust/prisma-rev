//! `prism-terrain-tool` — Terrain tooling for PrismaRev.
//!
//! Provides procedural terrain generation (noise, heightmap, erosion) and
//! image export.  Used by the asset pipeline and usable standalone.

pub mod erosion;
pub mod export;
pub mod heightmap;
pub mod noise;
