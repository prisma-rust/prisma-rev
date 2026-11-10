//! Tiled (.tmx / .tmj) 2D map parser, collision contour consolidation, and cooker.
//!
//! Features:
//! - Parses official Tiled JSON (.tmj) and XML (.tmx) map files
//! - Extracts multiple tile layers and object layers
//! - Greedy 2D box collision contour consolidation: merges adjacent solid tiles into minimal bounding boxes
//! - Emits binary `.rtilemap` format level assets for runtime consumption

use crate::cooker::CookError;
use prism_asset::formats::{
    encode_rtilemap, MergedCollisionBox, RtilemapInfo, TilemapLayer, TilemapObject,
};
use serde::{Deserialize, Serialize};

/// Intermediate representation of a parsed Tiled map.
#[derive(Debug, Clone, PartialEq)]
pub struct TiledMap {
    pub width: u32,
    pub height: u32,
    pub tile_width: u32,
    pub tile_height: u32,
    pub tileset_path: String,
    pub tile_layers: Vec<TiledTileLayer>,
    pub object_layers: Vec<TiledObjectLayer>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TiledTileLayer {
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub visible: bool,
    pub opacity: f32,
    pub offset: [f32; 2],
    pub data: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TiledObjectLayer {
    pub name: String,
    pub objects: Vec<TilemapObject>,
}

// ---------------------------------------------------------------------------
// JSON (.tmj) Schema Deserialization
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
struct TmjMap {
    width: u32,
    height: u32,
    tilewidth: u32,
    tileheight: u32,
    #[serde(default)]
    tilesets: Vec<TmjTilesetRef>,
    #[serde(default)]
    layers: Vec<TmjLayer>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct TmjTilesetRef {
    #[serde(default)]
    firstgid: u32,
    #[serde(default)]
    source: String,
    #[serde(default)]
    image: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct TmjLayer {
    name: String,
    #[serde(rename = "type")]
    layer_type: String,
    #[serde(default = "default_true")]
    visible: bool,
    #[serde(default = "default_one")]
    opacity: f32,
    #[serde(default)]
    offsetx: f32,
    #[serde(default)]
    offsety: f32,
    #[serde(default)]
    width: u32,
    #[serde(default)]
    height: u32,
    #[serde(default)]
    data: Vec<u32>,
    #[serde(default)]
    objects: Vec<TmjObject>,
}

fn default_true() -> bool {
    true
}

fn default_one() -> f32 {
    1.0
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct TmjObject {
    #[serde(default)]
    id: u32,
    #[serde(default)]
    name: String,
    #[serde(rename = "type", default)]
    class_name: String,
    #[serde(default)]
    x: f32,
    #[serde(default)]
    y: f32,
    #[serde(default)]
    width: f32,
    #[serde(default)]
    height: f32,
    #[serde(default)]
    rotation: f32,
    #[serde(default = "default_true")]
    visible: bool,
}

impl TiledMap {
    /// Parse a Tiled JSON (.tmj) formatted map string.
    pub fn from_tmj_str(json: &str) -> Result<Self, CookError> {
        let tmj: TmjMap = serde_json::from_str(json)
            .map_err(|e| CookError::CookFailed(format!("Failed to parse Tiled JSON: {e}")))?;

        let tileset_path = tmj
            .tilesets
            .first()
            .map(|t| {
                if !t.image.is_empty() {
                    t.image.clone()
                } else {
                    t.source.clone()
                }
            })
            .unwrap_or_default();

        let mut tile_layers = Vec::new();
        let mut object_layers = Vec::new();

        for l in tmj.layers {
            match l.layer_type.as_str() {
                "tilelayer" => {
                    let w = if l.width > 0 { l.width } else { tmj.width };
                    let h = if l.height > 0 { l.height } else { tmj.height };
                    tile_layers.push(TiledTileLayer {
                        name: l.name,
                        width: w,
                        height: h,
                        visible: l.visible,
                        opacity: l.opacity,
                        offset: [l.offsetx, l.offsety],
                        data: l.data,
                    });
                }
                "objectgroup" => {
                    let mut objs = Vec::with_capacity(l.objects.len());
                    for o in l.objects {
                        objs.push(TilemapObject {
                            id: o.id,
                            name: o.name,
                            class_name: o.class_name,
                            x: o.x,
                            y: o.y,
                            width: o.width,
                            height: o.height,
                            rotation: o.rotation,
                            visible: o.visible,
                        });
                    }
                    object_layers.push(TiledObjectLayer {
                        name: l.name,
                        objects: objs,
                    });
                }
                _ => {}
            }
        }

        Ok(Self {
            width: tmj.width,
            height: tmj.height,
            tile_width: tmj.tilewidth,
            tile_height: tmj.tileheight,
            tileset_path,
            tile_layers,
            object_layers,
        })
    }

    /// Parse a basic Tiled XML (.tmx) map string with CSV encoded layer data.
    pub fn from_tmx_str(xml: &str) -> Result<Self, CookError> {
        let width = extract_attr_u32(xml, "map", "width").unwrap_or(0);
        let height = extract_attr_u32(xml, "map", "height").unwrap_or(0);
        let tile_width = extract_attr_u32(xml, "map", "tilewidth").unwrap_or(16);
        let tile_height = extract_attr_u32(xml, "map", "tileheight").unwrap_or(16);

        if width == 0 || height == 0 {
            return Err(CookError::CookFailed("Invalid TMX map dimensions".into()));
        }

        let tileset_path = extract_attr_str(xml, "tileset", "source")
            .or_else(|| extract_attr_str(xml, "image", "source"))
            .unwrap_or_default();

        let mut tile_layers = Vec::new();

        // Extract <layer> tags
        let mut cur_search = xml;
        while let Some(start) = cur_search.find("<layer ") {
            let rest = &cur_search[start..];
            let end = rest.find("</layer>").unwrap_or(rest.len());
            let layer_xml = &rest[..end];
            let name = extract_attr_str(layer_xml, "layer", "name").unwrap_or_else(|| "Layer".into());
            let lw = extract_attr_u32(layer_xml, "layer", "width").unwrap_or(width);
            let lh = extract_attr_u32(layer_xml, "layer", "height").unwrap_or(height);

            // Extract CSV data inside <data> ... </data>
            let mut data = Vec::with_capacity((lw * lh) as usize);
            if let Some(d_start) = layer_xml.find("<data") {
                if let Some(content_start) = layer_xml[d_start..].find('>') {
                    let c_start = d_start + content_start + 1;
                    if let Some(c_end) = layer_xml[c_start..].find("</data>") {
                        let csv_str = &layer_xml[c_start..c_start + c_end];
                        for item in csv_str.split(',') {
                            let trimmed = item.trim();
                            if !trimmed.is_empty() {
                                if let Ok(gid) = trimmed.parse::<u32>() {
                                    data.push(gid);
                                }
                            }
                        }
                    }
                }
            }

            tile_layers.push(TiledTileLayer {
                name,
                width: lw,
                height: lh,
                visible: true,
                opacity: 1.0,
                offset: [0.0, 0.0],
                data,
            });

            cur_search = &rest[end..];
        }

        Ok(Self {
            width,
            height,
            tile_width,
            tile_height,
            tileset_path,
            tile_layers,
            object_layers: Vec::new(),
        })
    }
}

fn extract_attr_str(xml: &str, tag: &str, attr: &str) -> Option<String> {
    let tag_pattern = format!("<{tag} ");
    let tag_pos = xml.find(&tag_pattern)?;
    let tag_str = &xml[tag_pos..];
    let tag_end = tag_str.find('>')?;
    let tag_attrs = &tag_str[..tag_end];

    let attr_pattern = format!("{attr}=\"");
    let attr_pos = tag_attrs.find(&attr_pattern)?;
    let val_start = attr_pos + attr_pattern.len();
    let val_end = tag_attrs[val_start..].find('"')?;
    Some(tag_attrs[val_start..val_start + val_end].to_string())
}

fn extract_attr_u32(xml: &str, tag: &str, attr: &str) -> Option<u32> {
    extract_attr_str(xml, tag, attr)?.parse().ok()
}

// ---------------------------------------------------------------------------
// Greedy 2D Box Collision Contour Consolidation
// ---------------------------------------------------------------------------

/// Consolidate adjacent solid tiles in a 2D grid into the minimum number of
/// non-overlapping rectangular collision boxes using greedy 2D box merging.
pub fn merge_tile_collision_boxes(
    width: u32,
    height: u32,
    tile_w: f32,
    tile_h: f32,
    solid_grid: &[bool],
) -> Vec<MergedCollisionBox> {
    if width == 0 || height == 0 || solid_grid.len() < (width * height) as usize {
        return Vec::new();
    }

    let mut visited = vec![false; (width * height) as usize];
    let mut boxes = Vec::new();

    for y in 0..height {
        for x in 0..width {
            let idx = (y * width + x) as usize;
            if !solid_grid[idx] || visited[idx] {
                continue;
            }

            // Expand horizontally to find maximum width
            let mut w = 1;
            while x + w < width {
                let next_idx = (y * width + (x + w)) as usize;
                if solid_grid[next_idx] && !visited[next_idx] {
                    w += 1;
                } else {
                    break;
                }
            }

            // Expand vertically row by row while all cells in the span (x..x+w) are solid & unvisited
            let mut h = 1;
            while y + h < height {
                let mut row_solid = true;
                for dx in 0..w {
                    let check_idx = ((y + h) * width + (x + dx)) as usize;
                    if !solid_grid[check_idx] || visited[check_idx] {
                        row_solid = false;
                        break;
                    }
                }
                if row_solid {
                    h += 1;
                } else {
                    break;
                }
            }

            // Mark the rectangular merged region as visited
            for dy in 0..h {
                for dx in 0..w {
                    let mark_idx = ((y + dy) * width + (x + dx)) as usize;
                    visited[mark_idx] = true;
                }
            }

            boxes.push(MergedCollisionBox {
                rect: [
                    x as f32 * tile_w,
                    y as f32 * tile_h,
                    w as f32 * tile_w,
                    h as f32 * tile_h,
                ],
            });
        }
    }

    boxes
}

/// Cook a parsed Tiled map into binary `.rtilemap` format level asset.
pub fn cook_tiled_map(
    map: &TiledMap,
    tileset_override: Option<&str>,
    collision_layer_name: Option<&str>,
) -> Result<RtilemapInfo, CookError> {
    let tileset_path = tileset_override
        .unwrap_or(&map.tileset_path)
        .to_string();

    let mut layers = Vec::with_capacity(map.tile_layers.len());
    let mut solid_grid = vec![false; (map.width * map.height) as usize];

    for l in &map.tile_layers {
        layers.push(TilemapLayer {
            name: l.name.clone(),
            width: l.width,
            height: l.height,
            visible: l.visible,
            opacity: l.opacity,
            offset: l.offset,
            tiles: l.data.clone(),
        });

        // If this layer is designated as collision or named "Collision" / "Collisions",
        // mark its non-zero tiles as solid.
        let is_collision_layer = match collision_layer_name {
            Some(target) => l.name.eq_ignore_ascii_case(target),
            None => {
                l.name.eq_ignore_ascii_case("collision")
                    || l.name.eq_ignore_ascii_case("collisions")
                    || l.name.eq_ignore_ascii_case("solid")
            }
        };

        if is_collision_layer {
            for (idx, &gid) in l.data.iter().enumerate() {
                if gid > 0 && idx < solid_grid.len() {
                    solid_grid[idx] = true;
                }
            }
        }
    }

    let mut objects = Vec::new();
    for ol in &map.object_layers {
        for o in &ol.objects {
            objects.push(o.clone());
        }
    }

    // Consolidate adjacent solid tiles into rectangular collision boxes
    let collision_boxes = merge_tile_collision_boxes(
        map.width,
        map.height,
        map.tile_width as f32,
        map.tile_height as f32,
        &solid_grid,
    );

    Ok(RtilemapInfo {
        width: map.width,
        height: map.height,
        tile_width: map.tile_width,
        tile_height: map.tile_height,
        tileset_path,
        layers,
        objects,
        collision_boxes,
    })
}

/// Serialize cooked tilemap info into binary `.rtilemap` bytes.
pub fn encode_rtilemap_bytes(info: &RtilemapInfo) -> Vec<u8> {
    encode_rtilemap(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_greedy_collision_consolidation() {
        // 4x4 grid:
        // 1 1 0 0
        // 1 1 0 0
        // 1 1 1 1
        // 0 0 0 0
        let grid = vec![
            true, true, false, false,
            true, true, false, false,
            true, true, true, true,
            false, false, false, false,
        ];

        let boxes = merge_tile_collision_boxes(4, 4, 16.0, 16.0, &grid);
        // Should consolidate into 2 boxes:
        // 1) (0, 0) of size 2x3 (32x48)
        // 2) (2, 2) of size 2x1 (32x16)
        assert_eq!(boxes.len(), 2, "Adjacent tiles should consolidate greedily");
        assert_eq!(boxes[0].rect, [0.0, 0.0, 32.0, 48.0]);
        assert_eq!(boxes[1].rect, [32.0, 32.0, 32.0, 16.0]);
    }

    #[test]
    fn test_parse_tmj_and_cook() {
        let tmj_str = r#"{
            "width": 10,
            "height": 10,
            "tilewidth": 16,
            "tileheight": 16,
            "tilesets": [
                { "firstgid": 1, "image": "tiles/dungeon.rtex" }
            ],
            "layers": [
                {
                    "name": "Ground",
                    "type": "tilelayer",
                    "data": [1, 2, 3, 0, 0, 0, 0, 0, 0, 0],
                    "visible": true,
                    "opacity": 1.0
                },
                {
                    "name": "Collision",
                    "type": "tilelayer",
                    "data": [1, 1, 1, 0, 0, 0, 0, 0, 0, 0],
                    "visible": true
                },
                {
                    "name": "Spawns",
                    "type": "objectgroup",
                    "objects": [
                        {
                            "id": 1,
                            "name": "PlayerStart",
                            "type": "Spawn",
                            "x": 32.0,
                            "y": 48.0,
                            "width": 16.0,
                            "height": 16.0
                        }
                    ]
                }
            ]
        }"#;

        let map = TiledMap::from_tmj_str(tmj_str).expect("TMJ parsing should succeed");
        assert_eq!(map.width, 10);
        assert_eq!(map.tile_layers.len(), 2);
        assert_eq!(map.object_layers.len(), 1);

        let cooked = cook_tiled_map(&map, None, None).expect("Cook should succeed");
        assert_eq!(cooked.tileset_path, "tiles/dungeon.rtex");
        assert_eq!(cooked.layers.len(), 2);
        assert_eq!(cooked.objects.len(), 1);
        assert_eq!(cooked.objects[0].name, "PlayerStart");
        // Collision layer has 3 consecutive solid tiles: (0, 0), (1, 0), (2, 0)
        assert_eq!(cooked.collision_boxes.len(), 1);
        assert_eq!(cooked.collision_boxes[0].rect, [0.0, 0.0, 48.0, 16.0]);
    }

    #[test]
    fn test_parse_tmx_basic() {
        let tmx_str = r#"<?xml version="1.0" encoding="UTF-8"?>
        <map width="4" height="2" tilewidth="16" tileheight="16">
            <tileset source="dungeon.rtex"/>
            <layer name="Ground" width="4" height="2">
                <data encoding="csv">
                    1,2,3,4,
                    5,6,7,8
                </data>
            </layer>
        </map>"#;

        let map = TiledMap::from_tmx_str(tmx_str).expect("TMX parsing should succeed");
        assert_eq!(map.width, 4);
        assert_eq!(map.height, 2);
        assert_eq!(map.tile_layers.len(), 1);
        assert_eq!(map.tile_layers[0].data, vec![1, 2, 3, 4, 5, 6, 7, 8]);
    }
}
