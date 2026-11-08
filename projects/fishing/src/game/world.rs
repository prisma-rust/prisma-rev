//! 湖面世界：分层视差背景 + 昼夜换图。
//!
//! 这是**用户项目**的内容（关卡编排），不是引擎能力：
//! `assets/data/lake_layers.json` 描述湖面的图层顺序与视差系数，本模块为
//! 「当前时段」的每一层生成一个精灵实体，然后：
//!
//! - 每帧按相机位置平移各层（`pos = base + (cam - origin) * parallax`），
//!   远层几乎钉在屏幕上，近层跟随世界；
//! - 按 `cycle_seconds` 轮换 dawn → day → dusk → night：只换贴图路径并让
//!   资产系统重新解析（`SpriteRef.generation = 0`），不重复生成实体。
//!
//! 图层列表与顺序见 `assets/data/lake_layers.json`；美术由
//! `scripts/gen_placeholder_sprites.py` 生成，文件名与场景引用一致。
//!
//! 状态与读写都走 [`prism_game_api::GameWorld`]（与 `boat.rs` 同构），所以这份
//! 关卡逻辑既能编译进静态 Player，也能原样作为编辑器加载的动态模块运行。

use prism_game_api::GameWorld;
use prism_scene::components::{
    Active, Camera, LocalTransform, SpriteRenderer, TimeOfDay, TimeOfDayPhase,
};
use serde::Deserialize;

/// 每世界单位的像素数：1 像素 = 1 世界单位，相机正交半高 180 → 视口 360
/// 美术像素（640×360 虚拟分辨率）。
const PIXELS_PER_UNIT: f32 = 1.0;
/// 一个完整昼夜的时长（秒）。
const CYCLE_SECONDS: f32 = 48.0;
/// 背景层所在的 z（在船与水面之后）。
const BG_Z: f32 = -20.0;

#[derive(Deserialize)]
struct LayersFile {
    layers: Vec<LayerDef>,
    time_of_day: Vec<String>,
    /// 水面所在的世界 y（反射的镜像轴）。
    water_line: f32,
    /// 编排时相机所在位置；视差以它为原点。
    camera_origin: [f32; 2],
}

#[derive(Deserialize)]
struct LayerDef {
    slug: String,
    parallax: f32,
    /// 该层底边（轴心在底部中心）在 `camera_origin` 下的世界坐标。
    x: f32,
    y: f32,
    /// 是否在水面下生成一份镜像（侧面视角的屏幕反射）。
    #[serde(default)]
    reflect: bool,
    /// 均匀缩放（补偿素材高度与视口高度的差异），默认 1。
    #[serde(default = "unit_scale")]
    scale: f32,
    /// 排序层；默认等于图层下标（远 → 近）。前景层要盖住水面时显式指定。
    #[serde(default)]
    sorting: Option<i32>,
}

/// 单个图层实体。
struct LayerInstance {
    entity: prism_game_api::EntityHandle,
    base: glam::Vec3,
    parallax: f32,
    slug: String,
    /// 水面下的镜像实体（同层同视差，y 由水面镜像得到）。
    reflection: Option<prism_game_api::EntityHandle>,
}

/// 湖面状态——**项目私有**，由调用方持有（Player 侧放在 ECS resource 里）。
pub struct LakeWorld {
    camera: Option<prism_game_api::EntityHandle>,
    layers: Vec<LayerInstance>,
    tods: Vec<String>,
    current_tod: usize,
    elapsed: f32,
    /// 距离上次写「当前时刻」到存档的秒数（#984：重开游戏昼夜不会跳回早上）。
    clock_save_timer: f32,
    /// 本次运行是否已经从存档恢复过时刻。
    clock_restored: bool,
    /// 视差原点（编排时的相机位置）。
    camera_origin: glam::Vec2,
    /// 水面世界 y（反射镜像轴）。
    water_line: f32,
}

impl LakeWorld {
    /// 当前图层数量（Play 日志 / 冒烟测试用来确认世界真的建起来了）。
    pub fn layer_count(&self) -> usize {
        self.layers.len()
    }

    fn build(world: &mut GameWorld<'_>) -> Self {
        let camera = world.query::<Camera>().first().copied();

        let (layers_def, tods, camera_origin, water_line) = match load_layers(world) {
            Ok(file) => (
                file.layers,
                file.time_of_day,
                file.camera_origin,
                file.water_line,
            ),
            Err(err) => {
                log::error!("data/lake_layers.json: {err}");
                (Vec::new(), Vec::new(), [0.0, 0.0], 0.0)
            }
        };
        let camera_origin = glam::Vec2::new(camera_origin[0], camera_origin[1]);
        let current_tod = initial_tod(&tods);
        let tod_name = tods.get(current_tod).cloned().unwrap_or_default();

        let mut layers = Vec::new();
        for (order, def) in layers_def.iter().enumerate() {
            let path = layer_path(world, &def.slug, &tod_name);
            let entity = world.spawn();
            let base = glam::Vec3::new(def.x, def.y, BG_Z + order as f32 * 0.01);
            world.set(
                entity,
                LocalTransform {
                    position: base,
                    scale: glam::Vec3::splat(def.scale),
                    ..LocalTransform::default()
                },
            );
            world.set(
                entity,
                SpriteRenderer {
                    sprite_path: path.clone(),
                    pixels_per_unit: PIXELS_PER_UNIT,
                    // 底边中心对齐 `y`（轴心约定：(0.5, 0.0) = 底边）。
                    pivot: [0.5, 0.0],
                    sorting_layer: def.sorting.unwrap_or(order as i32),
                    receive_lights: true,
                    ..SpriteRenderer::default()
                },
            );
            world.set(entity, Active(true));

            // 水面反射：同一张贴图上下翻转、轴心改为顶部中心，
            // 位置由 tick 按 `y = 2 * water_line - source.y` 镜像。
            let reflection = def.reflect.then(|| {
                let mirror = world.spawn();
                world.set(
                    mirror,
                    LocalTransform {
                        position: glam::Vec3::new(def.x, 2.0 * water_line - def.y, base.z + 0.005),
                        ..LocalTransform::default()
                    },
                );
                world.set(
                    mirror,
                    SpriteRenderer {
                        sprite_path: path.clone(),
                        pixels_per_unit: PIXELS_PER_UNIT,
                        // 镜像层同样以顶边对齐水面镜像线，向下展开。
                        pivot: [0.5, 1.0],
                        flip_y: true,
                        // 镜像要压在水面之上才有反射感：水面在 20，船在 50。
                        sorting_layer: def.sorting.unwrap_or(order as i32) + 30,
                        color: [0.45, 0.6, 0.85, 0.5],
                        receive_lights: true,
                        ..SpriteRenderer::default()
                    },
                );
                world.set(mirror, Active(true));
                mirror
            });

            layers.push(LayerInstance {
                entity,
                base,
                parallax: def.parallax,
                slug: def.slug.clone(),
                reflection,
            });
        }

        log::info!(
            "lake world: {} layer(s), start time-of-day '{}'",
            layers_def.len(),
            tod_name
        );

        Self {
            camera,
            layers,
            tods,
            current_tod,
            elapsed: 0.0,
            clock_save_timer: 0.0,
            clock_restored: false,
            camera_origin,
            water_line,
        }
    }

    /// 切换时段：只换贴图路径并让资产系统重新解析。
    fn apply_time_of_day(&self, world: &mut GameWorld<'_>, tod: &str) {
        for layer in &self.layers {
            let path = layer_path(world, &layer.slug, tod);
            for entity in std::iter::once(layer.entity).chain(layer.reflection) {
                if let Some(mut sprite) = world.get::<SpriteRenderer>(entity) {
                    if sprite.sprite_path == path {
                        continue;
                    }
                    sprite.sprite_path = path.clone();
                    world.set(entity, sprite);
                }
            }
        }
    }
}

fn unit_scale() -> f32 {
    1.0
}

/// 图层贴图路径；该时段缺图时回落到 `day`（再缺就用该时段的名字，
/// 解析失败只是不画，不会崩）。
fn layer_path(world: &GameWorld<'_>, slug: &str, tod: &str) -> String {
    let candidate = format!("sprites/bg/{slug}@{tod}.png");
    if world.asset_exists(&candidate) {
        return candidate;
    }
    let fallback = format!("sprites/bg/{slug}@day.png");
    if world.asset_exists(&fallback) {
        return fallback;
    }
    candidate
}

/// 初始时段取 `day`，没有就取第一个。
fn initial_tod(tods: &[String]) -> usize {
    tods.iter().position(|t| t == "day").unwrap_or(0)
}

fn load_layers(world: &GameWorld<'_>) -> Result<LayersFile, String> {
    let text = world
        .read_asset_text("data/lake_layers.json")
        .map_err(|e| format!("read data/lake_layers.json: {e}"))?;
    serde_json::from_str(&text).map_err(|e| format!("parse lake_layers.json: {e}"))
}

/// 每帧：推进昼夜、平移各层。
/// 引擎 `TimeOfDay` 的阶段 → `data/lake_layers.json` 里 `time_of_day` 的名字。
fn lake_phase(world: &mut GameWorld<'_>) -> Option<&'static str> {
    let clock = world
        .query::<TimeOfDay>()
        .into_iter()
        .find_map(|entity| world.get::<TimeOfDay>(entity))?;
    Some(match clock.phase() {
        TimeOfDayPhase::Dawn => "dawn",
        TimeOfDayPhase::Day => "day",
        TimeOfDayPhase::Dusk => "dusk",
        TimeOfDayPhase::Night => "night",
    })
}

pub fn tick(state: &mut Option<LakeWorld>, world: &mut GameWorld<'_>, dt: f32) {
    if state.is_none() {
        *state = Some(LakeWorld::build(world));
    }
    let Some(current) = state.as_mut() else {
        return;
    };

    // #984：首次进入时把存档里的时刻写回 `TimeOfDay`（重开继续上次的昼夜）。
    if !current.clock_restored {
        current.clock_restored = true;
        if let Some(saved) = world
            .load_data("hours")
            .and_then(|value| value.parse::<f32>().ok())
        {
            if let Some(entity) = world
                .query::<TimeOfDay>()
                .into_iter()
                .find(|entity| world.get::<TimeOfDay>(*entity).is_some())
            {
                let mut clock = world.get::<TimeOfDay>(entity).expect("clock");
                clock.hours = saved.rem_euclid(24.0);
                world.set(entity, clock);
                log::info!("time of day restored: {}", clock.clock_text());
            }
        }
    }

    current.elapsed += dt;

    // --- 昼夜轮换 -------------------------------------------------------
    //
    // #983：时刻来自引擎的 `TimeOfDay` 组件，工程不再自己算 48 秒一圈 ——
    // 环境光（同一实体上的 `Light2D`）也由引擎系统按同一个时钟推进，
    // 所以「换背景图」和「变暗」永远同步。
    if !current.tods.is_empty() {
        if let Some(phase) = lake_phase(world) {
            let tod = current
                .tods
                .iter()
                .position(|name| name.eq_ignore_ascii_case(phase))
                .unwrap_or(current.current_tod)
                .min(current.tods.len().saturating_sub(1));
            if tod != current.current_tod {
                current.current_tod = tod;
                let name = current.tods[tod].clone();
                log::info!("time of day -> {name}");
                current.apply_time_of_day(world, &name);
            }
        }
    }

    // --- 保存时刻（每 2 秒一次，退出不会丢昼夜）-----------------------------
    current.clock_save_timer += dt;
    if current.clock_save_timer >= 2.0 {
        current.clock_save_timer = 0.0;
        if let Some(clock) = world
            .query::<TimeOfDay>()
            .into_iter()
            .find_map(|entity| world.get::<TimeOfDay>(entity))
        {
            let hours = format!("{:.3}", clock.normalized_hours());
            world.save_data("hours", &hours);
        }
    }

    // --- 视差 -----------------------------------------------------------
    let camera_pos = current
        .camera
        .and_then(|camera| world.get::<LocalTransform>(camera))
        .map(|transform| transform.position)
        .unwrap_or_default();

    for layer in &current.layers {
        let x = layer.base.x + (camera_pos.x - current.camera_origin.x) * layer.parallax;
        let y = layer.base.y + (camera_pos.y - current.camera_origin.y) * layer.parallax;
        if let Some(mut transform) = world.get::<LocalTransform>(layer.entity) {
            transform.position.x = x;
            transform.position.y = y;
            world.set(layer.entity, transform);
        }
        if let Some(mirror) = layer.reflection {
            if let Some(mut transform) = world.get::<LocalTransform>(mirror) {
                transform.position.x = x;
                // 镜像顶边 = 源层底边（轴心在顶部中心）。山体底边本来就埋在水面
                // 之下用来藏贴图的裁切边，所以按水面翻转会让镜像越过水线露在外面；
                // 取 `min` 让底边高于水面的层仍然贴着水面翻转。
                transform.position.y = y.min(current.water_line);
                world.set(mirror, transform);
            }
        }
    }
}
