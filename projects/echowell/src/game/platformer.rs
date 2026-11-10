//! EchoWell 的确定性平台跳跃切片。
//!
//! 玩法状态放在项目侧，渲染和输入仍由 PrismaRev 提供。`step` 是纯状态更新
//! 入口，测试和输入回放都调用它；`tick` 只负责把状态绑定到场景实体。

use prism_game_api::physics2d::{
    move_and_slide, move_and_slide_with_options, raycast_aabb, Aabb2, MoveOptions2D, MoveResult2,
    Obstacle2D,
};
use prism_game_api::{
    keys, ActionFrame, EntityHandle, GameWorld, InputActionMap, InputSnapshot, TriggerPhase2D,
};
use prism_scene::components::{
    Active, Camera2D, Collider2D, Hitbox2D, Hurtbox2D, Light2D, Light2DKind, LocalTransform, Name,
    ParticleEmitter2D, SpriteAnimation, SpriteRenderer, Tilemap, TilemapCollider2D,
};
use prism_scene::ui::TextAlign;
use prism_scene::ui::{Anchors, Canvas, CanvasScaleMode, Margin, Node, Pivot, Style, Text};
use serde::{Deserialize, Serialize};

pub const FIXED_DT: f32 = 1.0 / 60.0;
// Function keys use the project API's raw virtual-key slots.
const F1_KEY_INDEX: usize = keys::OTHER_BASE + 0x70;
const SOLID_LAYER: u32 = 1;
const HAZARD_LAYER: u32 = 2;
const PICKUP_LAYER: u32 = 4;
const ENEMY_LAYER: u32 = 8;
const EXIT_LAYER: u32 = 16;
const DASH_HITBOX_LAYER: u32 = 32;
const PLAYER_TEAM: u32 = 1;
const ENEMY_TEAM: u32 = 2;
const DASH_HITBOX_HALF_EXTENTS: [f32; 2] = [8.0, 6.0];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Phase {
    #[default]
    Normal,
    Dash,
    Dead,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AbilityFlags(u32);

impl AbilityFlags {
    pub const WALL_JUMP: u32 = 1 << 0;
    pub const DASH: u32 = 1 << 1;
    pub const DOUBLE_JUMP: u32 = 1 << 2;

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn with_dash() -> Self {
        Self(Self::DASH)
    }

    pub const fn from_bits(bits: u32) -> Self {
        Self(bits)
    }

    pub const fn contains(self, flag: u32) -> bool {
        self.0 & flag != 0
    }

    pub fn insert(&mut self, flag: u32) {
        self.0 |= flag;
    }

    pub const fn bits(self) -> u32 {
        self.0
    }
}

fn ability_flag_name(ability: u32) -> &'static str {
    match ability {
        AbilityFlags::WALL_JUMP => "echowell.ability.wall_jump",
        AbilityFlags::DASH => "echowell.ability.dash",
        AbilityFlags::DOUBLE_JUMP => "echowell.ability.double_jump",
        _ => "echowell.ability.unknown",
    }
}

/// 由配置文件覆盖的手感参数。数值默认与需求文档的量级一致。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct Tuning {
    pub fixed_hz: f32,
    pub max_run_speed: f32,
    pub ground_acceleration: f32,
    pub air_acceleration: f32,
    pub gravity: f32,
    pub max_fall_speed: f32,
    pub jump_speed: f32,
    pub coyote_seconds: f32,
    pub jump_buffer_seconds: f32,
    pub variable_jump_seconds: f32,
    pub dash_speed: f32,
    pub dash_seconds: f32,
    pub dash_end_speed: f32,
    pub wall_jump_speed: f32,
    pub wall_jump_horizontal_speed: f32,
    pub respawn_seconds: f32,
    pub player_half_size: [f32; 2],
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            fixed_hz: 60.0,
            max_run_speed: 90.0,
            ground_acceleration: 1_000.0,
            air_acceleration: 650.0,
            gravity: 900.0,
            max_fall_speed: 160.0,
            jump_speed: 300.0,
            coyote_seconds: 0.10,
            jump_buffer_seconds: 0.08,
            variable_jump_seconds: 0.20,
            dash_speed: 240.0,
            dash_seconds: 0.15,
            dash_end_speed: 160.0,
            wall_jump_speed: 280.0,
            wall_jump_horizontal_speed: 150.0,
            respawn_seconds: 0.50,
            player_half_size: [6.0, 8.0],
        }
    }
}

type Aabb = Aabb2;

#[derive(Debug, Clone)]
pub struct EnemyDefinition {
    pub id: String,
    pub spawn: [f32; 2],
    pub patrol: [f32; 4],
    pub speed: f32,
    pub half_size: [f32; 2],
    pub health: u32,
}

#[derive(Debug, Clone)]
pub struct EnemyState {
    pub definition: EnemyDefinition,
    pub position: [f32; 2],
    pub direction: f32,
    pub health: u32,
    pub alive: bool,
}

impl EnemyState {
    fn new(definition: EnemyDefinition) -> Self {
        Self {
            position: definition.spawn,
            direction: 1.0,
            health: definition.health.max(1),
            alive: true,
            definition,
        }
    }

    fn reset(&mut self) {
        self.position = self.definition.spawn;
        self.direction = 1.0;
        self.health = self.definition.health.max(1);
        self.alive = true;
    }

    fn step(&mut self, dt: f32) {
        if !self.alive {
            return;
        }
        let half_width = self.definition.half_size[0].max(0.0);
        let min_x = self.definition.patrol[0] + half_width;
        let max_x = self.definition.patrol[2] - half_width;
        if max_x <= min_x {
            self.position[0] = (min_x + max_x) * 0.5;
            return;
        }
        self.position[0] += self.direction * self.definition.speed.max(0.0) * dt.max(0.0);
        if self.position[0] <= min_x {
            self.position[0] = min_x;
            self.direction = 1.0;
        } else if self.position[0] >= max_x {
            self.position[0] = max_x;
            self.direction = -1.0;
        }
    }
}

/// 一个房间的灰盒关卡数据。后续可直接由 Tilemap / Tiled cooker 生成同样的数组。
#[derive(Debug, Clone)]
pub struct WorldGeometry {
    pub solids: Vec<Aabb>,
    pub one_way_platforms: Vec<Aabb>,
    pub one_way_tile_ids: Vec<u32>,
    pub hazards: Vec<Aabb>,
    pub dash_gate: Aabb,
    pub crystal: [f32; 2],
    pub spawn: [f32; 2],
    pub columns: u32,
    pub tiles: Vec<u32>,
    pub enemies: Vec<EnemyDefinition>,
}

impl Default for WorldGeometry {
    fn default() -> Self {
        Self {
            solids: vec![
                Aabb::new(0.0, 0.0, 320.0, 16.0),
                Aabb::new(0.0, 16.0, 16.0, 192.0),
                Aabb::new(304.0, 80.0, 320.0, 192.0),
                Aabb::new(32.0, 32.0, 112.0, 48.0),
                Aabb::new(144.0, 64.0, 208.0, 80.0),
                Aabb::new(224.0, 32.0, 272.0, 48.0),
            ],
            one_way_platforms: Vec::new(),
            one_way_tile_ids: Vec::new(),
            hazards: vec![
                Aabb::new(110.0, 16.0, 140.0, 26.0),
                Aabb::new(270.0, 16.0, 302.0, 26.0),
            ],
            dash_gate: Aabb::new(296.0, 16.0, 304.0, 80.0),
            crystal: [82.0, 56.0],
            spawn: [24.0, 24.0],
            columns: 20,
            tiles: Vec::new(),
            enemies: Vec::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct RoomFile {
    #[serde(default)]
    id: String,
    #[serde(default = "default_room_bounds")]
    bounds: [f32; 4],
    hazards: Vec<[f32; 4]>,
    dash_gate: [f32; 4],
    crystal: [f32; 2],
    spawn: [f32; 2],
    columns: u32,
    tiles: Vec<u32>,
    #[serde(default)]
    one_way_tile_ids: Vec<u32>,
    #[serde(default)]
    exits: Vec<ExitFile>,
    #[serde(default)]
    enemies: Vec<EnemyFile>,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct EnemyFile {
    id: String,
    spawn: [f32; 2],
    patrol: [f32; 4],
    speed: f32,
    half_size: [f32; 2],
    health: u32,
}

impl Default for EnemyFile {
    fn default() -> Self {
        Self {
            id: String::new(),
            spawn: [0.0, 0.0],
            patrol: [0.0, 0.0, 0.0, 0.0],
            speed: 0.0,
            half_size: [6.0, 8.0],
            health: 1,
        }
    }
}

#[derive(Debug, Deserialize)]
struct ExitFile {
    rect: [f32; 4],
    target: String,
    entry: [f32; 2],
    #[serde(default)]
    required_ability: Option<String>,
}

#[derive(Debug, Clone)]
struct RoomExit {
    trigger: Aabb,
    target: String,
    entry: [f32; 2],
    required_ability: Option<u32>,
}

#[derive(Debug, Clone)]
struct RoomDefinition {
    id: String,
    bounds: [f32; 4],
    geometry: WorldGeometry,
    exits: Vec<RoomExit>,
}

fn default_room_bounds() -> [f32; 4] {
    [0.0, 0.0, 320.0, 180.0]
}

impl RoomFile {
    fn definition(self) -> RoomDefinition {
        let RoomFile {
            id,
            bounds,
            hazards,
            dash_gate,
            crystal,
            spawn,
            columns,
            tiles,
            one_way_tile_ids,
            exits,
            enemies,
        } = self;
        let tilemap = Tilemap {
            columns,
            cell_size: [16.0, 16.0],
            tiles: tiles.clone(),
            ..Tilemap::default()
        };
        let tilemap_collider = TilemapCollider2D {
            solid_tile_ids: vec![1, 2, 3],
            one_way_tile_ids: one_way_tile_ids.clone(),
            layer: SOLID_LAYER,
            ..TilemapCollider2D::default()
        };
        let shapes = prism_scene::components::tilemap_collider_shapes(
            &tilemap,
            &tilemap_collider,
            glam::Mat4::IDENTITY,
        );
        let solids = shapes
            .iter()
            .filter(|shape| !shape.one_way_top)
            .map(|shape| shape.bounds)
            .collect();
        let one_way_platforms = shapes
            .iter()
            .filter(|shape| shape.one_way_top)
            .map(|shape| shape.bounds)
            .collect();
        let id = if id.is_empty() {
            "well".to_string()
        } else {
            id
        };
        let exits = exits
            .into_iter()
            .filter_map(|exit| {
                let required_ability = match exit.required_ability.as_deref() {
                    None => None,
                    Some("dash") => Some(AbilityFlags::DASH),
                    Some("wall_jump") => Some(AbilityFlags::WALL_JUMP),
                    Some("double_jump") => Some(AbilityFlags::DOUBLE_JUMP),
                    Some(_) => return None,
                };
                Some(RoomExit {
                    trigger: aabb_from_array(exit.rect),
                    target: exit.target,
                    entry: exit.entry,
                    required_ability,
                })
            })
            .collect();
        RoomDefinition {
            id,
            bounds,
            geometry: WorldGeometry {
                solids,
                one_way_platforms,
                one_way_tile_ids,
                hazards: hazards.into_iter().map(aabb_from_array).collect(),
                dash_gate: aabb_from_array(dash_gate),
                crystal,
                spawn,
                columns,
                tiles,
                enemies: enemies
                    .into_iter()
                    .enumerate()
                    .map(|(index, enemy)| EnemyDefinition {
                        id: if enemy.id.is_empty() {
                            format!("enemy-{index}")
                        } else {
                            enemy.id
                        },
                        spawn: enemy.spawn,
                        patrol: enemy.patrol,
                        speed: enemy.speed,
                        half_size: enemy.half_size,
                        health: enemy.health.max(1),
                    })
                    .collect(),
            },
            exits,
        }
    }
}

#[derive(Debug, Deserialize)]
struct RoomManifest {
    rooms: Vec<RoomFile>,
}

fn load_rooms(world: &GameWorld<'_>) -> Vec<RoomDefinition> {
    let loaded = world
        .read_asset_text("rooms.json")
        .ok()
        .and_then(|text| serde_json::from_str::<RoomManifest>(&text).ok())
        .map(|manifest| manifest.rooms)
        .or_else(|| {
            world
                .read_asset_text("room.json")
                .ok()
                .and_then(|text| serde_json::from_str::<RoomFile>(&text).ok())
                .map(|room| vec![room])
        });
    let mut rooms: Vec<RoomDefinition> = loaded
        .unwrap_or_default()
        .into_iter()
        .map(RoomFile::definition)
        .filter(|room| {
            room.bounds.iter().all(|value| value.is_finite())
                && room.bounds[2] > room.bounds[0]
                && room.bounds[3] > room.bounds[1]
        })
        .collect();
    if rooms.is_empty() {
        rooms.push(RoomDefinition {
            id: "well".to_string(),
            bounds: default_room_bounds(),
            geometry: WorldGeometry::default(),
            exits: Vec::new(),
        });
    }
    if rooms.iter().any(|room| room.id.is_empty()) {
        rooms[0].id = "well".to_string();
    }
    let room_ids: std::collections::HashSet<String> =
        rooms.iter().map(|room| room.id.clone()).collect();
    for room in &mut rooms {
        room.exits.retain(|exit| room_ids.contains(&exit.target));
    }
    rooms
}

#[cfg(test)]
fn find_room_transition(
    rooms: &[RoomDefinition],
    current_room: usize,
    player_body: Aabb,
    abilities: AbilityFlags,
) -> Option<(usize, [f32; 2])> {
    let room = rooms.get(current_room)?;
    room.exits.iter().find_map(|exit| {
        if !player_body.overlaps(exit.trigger)
            || exit
                .required_ability
                .is_some_and(|ability| !abilities.contains(ability))
        {
            return None;
        }
        rooms
            .iter()
            .position(|target| target.id == exit.target)
            .map(|index| (index, exit.entry))
    })
}

fn can_transition_from_phase(phase: Phase) -> bool {
    phase != Phase::Dead
}

fn enter_room(player: &mut PlayerState, room: &RoomDefinition, entry: [f32; 2]) {
    player.room_id = room.id.clone();
    player.geometry = room.geometry.clone();
    player.enemies = player
        .geometry
        .enemies
        .iter()
        .cloned()
        .map(EnemyState::new)
        .collect();
    player.position = entry;
    player.grounded = false;
    player.coyote_timer = 0.0;
    player.jump_buffer_timer = 0.0;
    player.variable_jump_timer = 0.0;
    player.air_jumps_remaining = if player.abilities.contains(AbilityFlags::DOUBLE_JUMP) {
        1
    } else {
        0
    };
}

#[cfg(test)]
fn room_ids_resolve(room: &RoomDefinition, ids: &std::collections::HashSet<&str>) -> bool {
    room.exits
        .iter()
        .all(|exit| ids.contains(exit.target.as_str()))
}

fn is_grounded_at(position: [f32; 2], half_size: [f32; 2], solids: &[Aabb]) -> bool {
    move_and_slide(position, half_size, [0.0, -0.02], solids).grounded
}

fn aabb_from_array(value: [f32; 4]) -> Aabb {
    Aabb::new(value[0], value[1], value[2], value[3])
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InputFrame {
    pub left: bool,
    pub right: bool,
    pub up: bool,
    pub down: bool,
    pub jump_pressed: bool,
    pub jump_held: bool,
    pub dash_pressed: bool,
}

impl InputFrame {
    pub fn from_snapshot(input: InputSnapshot) -> Self {
        let mut map = platformer_input_map();
        Self::from_actions(map.sample(input))
    }

    fn from_actions(input: ActionFrame) -> Self {
        let direction = input.vector2("left", "right", "down", "up");
        let jump = input.action("jump");
        let dash = input.action("dash");
        Self {
            left: direction[0] < 0.0,
            right: direction[0] > 0.0,
            up: direction[1] > 0.0,
            down: direction[1] < 0.0,
            jump_pressed: jump.pressed,
            jump_held: jump.held,
            dash_pressed: dash.pressed,
        }
    }
}

fn platformer_input_map() -> InputActionMap {
    let mut map = InputActionMap::default();
    map.bind_keys("left", [keys::KEY_A, keys::ARROW_LEFT]);
    map.bind_keys("right", [keys::KEY_D, keys::ARROW_RIGHT]);
    map.bind_keys("up", [keys::KEY_W, keys::ARROW_UP]);
    map.bind_keys("down", [keys::KEY_S, keys::ARROW_DOWN]);
    map.bind_keys("jump", [keys::SPACE, keys::KEY_W]);
    map.bind_keys("dash", [keys::SHIFT_LEFT, keys::SHIFT_RIGHT, keys::KEY_Q]);
    map
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StepEvents {
    pub landed: bool,
    pub dashed: bool,
    pub died: bool,
    pub respawned: bool,
    pub hit_wall: bool,
    pub ability_unlocked: bool,
    pub enemy_defeated: bool,
}

/// 输入回放只保存逻辑帧，不保存渲染时间或实体句柄。
#[derive(Debug, Clone, Default)]
pub struct InputReplay {
    pub recording: bool,
    pub playing: bool,
    pub cursor: usize,
    pub frames: Vec<InputFrame>,
    start_state: Option<ReplaySnapshot>,
}

impl InputReplay {
    pub fn record(&mut self, frame: InputFrame) {
        if self.recording {
            self.frames.push(frame);
        }
    }

    pub fn begin_playback(&mut self) {
        self.cursor = 0;
        self.playing = !self.frames.is_empty();
        self.recording = false;
    }
}

impl Iterator for InputReplay {
    type Item = InputFrame;

    fn next(&mut self) -> Option<Self::Item> {
        if !self.playing {
            return None;
        }
        let frame = self.frames.get(self.cursor).copied();
        self.cursor = self.cursor.saturating_add(1);
        if frame.is_none() {
            self.playing = false;
        }
        frame
    }
}

#[derive(Debug, Clone)]
struct ReplaySnapshot {
    room_id: String,
    phase: Phase,
    position: [f32; 2],
    velocity: [f32; 2],
    facing: f32,
    grounded: bool,
    coyote_timer: f32,
    jump_buffer_timer: f32,
    variable_jump_timer: f32,
    dash_timer: f32,
    dash_available: bool,
    air_jumps_remaining: u8,
    respawn_timer: f32,
    deaths: u32,
    abilities: AbilityFlags,
    enemies: Vec<EnemyState>,
    tuning: Tuning,
    geometry: WorldGeometry,
}

impl ReplaySnapshot {
    fn capture(state: &PlayerState) -> Self {
        Self {
            room_id: state.room_id.clone(),
            phase: state.phase,
            position: state.position,
            velocity: state.velocity,
            facing: state.facing,
            grounded: state.grounded,
            coyote_timer: state.coyote_timer,
            jump_buffer_timer: state.jump_buffer_timer,
            variable_jump_timer: state.variable_jump_timer,
            dash_timer: state.dash_timer,
            dash_available: state.dash_available,
            air_jumps_remaining: state.air_jumps_remaining,
            respawn_timer: state.respawn_timer,
            deaths: state.deaths,
            abilities: state.abilities,
            enemies: state.enemies.clone(),
            tuning: state.tuning,
            geometry: state.geometry.clone(),
        }
    }

    fn restore(&self, state: &mut PlayerState) {
        state.room_id = self.room_id.clone();
        state.phase = self.phase;
        state.position = self.position;
        state.velocity = self.velocity;
        state.facing = self.facing;
        state.grounded = self.grounded;
        state.coyote_timer = self.coyote_timer;
        state.jump_buffer_timer = self.jump_buffer_timer;
        state.variable_jump_timer = self.variable_jump_timer;
        state.dash_timer = self.dash_timer;
        state.dash_available = self.dash_available;
        state.air_jumps_remaining = self.air_jumps_remaining;
        state.respawn_timer = self.respawn_timer;
        state.deaths = self.deaths;
        state.abilities = self.abilities;
        state.enemies.clone_from(&self.enemies);
        state.tuning = self.tuning;
        state.geometry = self.geometry.clone();
    }
}

#[derive(Debug, Clone)]
pub struct PlayerState {
    pub room_id: String,
    pub phase: Phase,
    pub position: [f32; 2],
    pub velocity: [f32; 2],
    pub facing: f32,
    pub grounded: bool,
    pub coyote_timer: f32,
    pub jump_buffer_timer: f32,
    pub variable_jump_timer: f32,
    pub dash_timer: f32,
    pub dash_available: bool,
    pub air_jumps_remaining: u8,
    pub respawn_timer: f32,
    pub deaths: u32,
    pub abilities: AbilityFlags,
    pub enemies: Vec<EnemyState>,
    pub tuning: Tuning,
    pub replay: InputReplay,
    pub geometry: WorldGeometry,
}

impl Default for PlayerState {
    fn default() -> Self {
        Self::new(Tuning::default(), WorldGeometry::default())
    }
}

impl PlayerState {
    pub fn new(tuning: Tuning, geometry: WorldGeometry) -> Self {
        Self::new_in_room("well", tuning, geometry)
    }

    fn new_in_room(room_id: &str, tuning: Tuning, geometry: WorldGeometry) -> Self {
        let position = geometry.spawn;
        let grounded = is_grounded_at(position, tuning.player_half_size, &geometry.solids);
        let enemies = geometry
            .enemies
            .iter()
            .cloned()
            .map(EnemyState::new)
            .collect();
        Self {
            room_id: room_id.to_string(),
            phase: Phase::Normal,
            position,
            velocity: [0.0, 0.0],
            facing: 1.0,
            grounded,
            coyote_timer: 0.0,
            jump_buffer_timer: 0.0,
            variable_jump_timer: 0.0,
            dash_timer: 0.0,
            dash_available: true,
            air_jumps_remaining: 0,
            respawn_timer: 0.0,
            deaths: 0,
            abilities: AbilityFlags::empty(),
            enemies,
            tuning,
            replay: InputReplay::default(),
            geometry,
        }
    }

    pub fn unlock(&mut self, ability: u32) {
        self.abilities.insert(ability);
        if ability == AbilityFlags::DASH {
            self.dash_available = true;
        }
        if ability == AbilityFlags::DOUBLE_JUMP {
            self.air_jumps_remaining = 1;
        }
    }

    pub fn begin_recording(&mut self) {
        self.replay.frames.clear();
        self.replay.cursor = 0;
        self.replay.playing = false;
        self.replay.recording = true;
        self.replay.start_state = Some(ReplaySnapshot::capture(self));
    }

    pub fn begin_playback(&mut self) {
        let start_state = self.replay.start_state.clone();
        if let Some(start_state) = start_state {
            start_state.restore(self);
        }
        self.replay.begin_playback();
    }

    pub fn debug_line(&self) -> String {
        format!(
            "{}  x:{:.1} y:{:.1}  vx:{:.1} vy:{:.1}  coy:{:.2} buf:{:.2}  deaths:{}",
            match self.phase {
                Phase::Normal => "NORMAL",
                Phase::Dash => "DASH",
                Phase::Dead => "DEAD",
            },
            self.position[0],
            self.position[1],
            self.velocity[0],
            self.velocity[1],
            self.coyote_timer,
            self.jump_buffer_timer,
            self.deaths
        )
    }

    pub fn step(&mut self, input: InputFrame, dt: f32) -> StepEvents {
        let dt = dt.clamp(0.0, 0.1);
        let solids: Vec<_> = self.active_solids().collect();
        let obstacles: Vec<_> = self.active_obstacles().collect();
        let hazards = self.geometry.hazards.clone();
        if let Some(recorded) = self.replay.next() {
            return self.step_inner(
                recorded,
                dt,
                |center, half, delta, options| {
                    move_and_slide_with_options(center, half, delta, &obstacles, options)
                },
                |body| hazards.iter().any(|hazard| body.overlaps(*hazard)),
                |origin, direction, distance| raycast_solids(origin, direction, distance, &solids),
            );
        }
        self.replay.record(input);
        self.step_inner(
            input,
            dt,
            |center, half, delta, options| {
                move_and_slide_with_options(center, half, delta, &obstacles, options)
            },
            |body| hazards.iter().any(|hazard| body.overlaps(*hazard)),
            |origin, direction, distance| raycast_solids(origin, direction, distance, &solids),
        )
    }

    fn step_with_world(
        &mut self,
        input: InputFrame,
        dt: f32,
        world: &GameWorld<'_>,
        entity: EntityHandle,
    ) -> StepEvents {
        let dt = dt.clamp(0.0, 0.1);
        if let Some(recorded) = self.replay.next() {
            return self.step_inner(
                recorded,
                dt,
                |center, half, delta, options| {
                    world.move_and_slide2d_with_options(
                        Some(entity),
                        center,
                        half,
                        delta,
                        SOLID_LAYER,
                        options,
                    )
                },
                |body| !world.overlap2d(body, HAZARD_LAYER).is_empty(),
                |origin, direction, distance| {
                    world
                        .raycast2d(origin, direction, distance, SOLID_LAYER)
                        .map(|hit| hit.distance)
                },
            );
        }
        self.replay.record(input);
        self.step_inner(
            input,
            dt,
            |center, half, delta, options| {
                world.move_and_slide2d_with_options(
                    Some(entity),
                    center,
                    half,
                    delta,
                    SOLID_LAYER,
                    options,
                )
            },
            |body| !world.overlap2d(body, HAZARD_LAYER).is_empty(),
            |origin, direction, distance| {
                world
                    .raycast2d(origin, direction, distance, SOLID_LAYER)
                    .map(|hit| hit.distance)
            },
        )
    }

    fn step_inner<M, H, R>(
        &mut self,
        input: InputFrame,
        dt: f32,
        mut move_body: M,
        overlaps_hazard: H,
        mut raycast: R,
    ) -> StepEvents
    where
        M: FnMut([f32; 2], [f32; 2], [f32; 2], MoveOptions2D) -> MoveResult2,
        H: Fn(Aabb) -> bool,
        R: FnMut([f32; 2], [f32; 2], f32) -> Option<f32>,
    {
        if self.phase == Phase::Dead {
            self.respawn_timer -= dt;
            if self.respawn_timer <= 0.0 {
                self.phase = Phase::Normal;
                self.position = self.geometry.spawn;
                self.velocity = [0.0, 0.0];
                self.grounded = is_grounded_at(
                    self.position,
                    self.tuning.player_half_size,
                    &self.geometry.solids,
                );
                self.coyote_timer = 0.0;
                self.jump_buffer_timer = 0.0;
                self.dash_available = true;
                self.air_jumps_remaining = if self.abilities.contains(AbilityFlags::DOUBLE_JUMP) {
                    1
                } else {
                    0
                };
                return StepEvents {
                    respawned: true,
                    ..StepEvents::default()
                };
            }
            return StepEvents::default();
        }

        let mut events = StepEvents::default();
        let drop_through_one_way = input.down && input.jump_pressed;
        if drop_through_one_way {
            self.jump_buffer_timer = 0.0;
        } else if input.jump_pressed {
            self.jump_buffer_timer = self.tuning.jump_buffer_seconds;
        } else {
            self.jump_buffer_timer = (self.jump_buffer_timer - dt).max(0.0);
        }

        if self.grounded {
            self.coyote_timer = self.tuning.coyote_seconds;
            self.dash_available = true;
            self.air_jumps_remaining = if self.abilities.contains(AbilityFlags::DOUBLE_JUMP) {
                1
            } else {
                0
            };
        } else {
            self.coyote_timer = (self.coyote_timer - dt).max(0.0);
        }

        if self.phase == Phase::Normal {
            if input.dash_pressed
                && self.abilities.contains(AbilityFlags::DASH)
                && self.dash_available
            {
                let mut direction = [
                    (input.right as i8 - input.left as i8) as f32,
                    (input.up as i8 - input.down as i8) as f32,
                ];
                if direction == [0.0, 0.0] {
                    direction = [self.facing, 0.0];
                }
                let length = (direction[0] * direction[0] + direction[1] * direction[1]).sqrt();
                direction[0] /= length.max(1.0);
                direction[1] /= length.max(1.0);
                if direction[0].abs() > 0.01 {
                    self.facing = direction[0].signum();
                }
                self.velocity = [
                    direction[0] * self.tuning.dash_speed,
                    direction[1] * self.tuning.dash_speed,
                ];
                self.dash_timer = self.tuning.dash_seconds;
                self.dash_available = false;
                self.phase = Phase::Dash;
                events.dashed = true;
            } else {
                let axis = (input.right as i8 - input.left as i8) as f32;
                if axis != 0.0 {
                    self.facing = axis.signum();
                }
                let acceleration = if self.grounded {
                    self.tuning.ground_acceleration
                } else {
                    self.tuning.air_acceleration
                };
                self.velocity[0] = approach(
                    self.velocity[0],
                    axis * self.tuning.max_run_speed,
                    acceleration * dt,
                );

                if self.jump_buffer_timer > 0.0 && (self.grounded || self.coyote_timer > 0.0) {
                    self.velocity[1] = self.tuning.jump_speed;
                    self.grounded = false;
                    self.coyote_timer = 0.0;
                    self.jump_buffer_timer = 0.0;
                    self.variable_jump_timer = self.tuning.variable_jump_seconds;
                } else if input.jump_pressed && !drop_through_one_way {
                    // Wall jump has priority over the air jump when both are unlocked.
                    let wall = if self.abilities.contains(AbilityFlags::WALL_JUMP) {
                        self.near_wall(&mut raycast)
                    } else {
                        0.0
                    };
                    if wall != 0.0 {
                        self.velocity = [
                            -wall * self.tuning.wall_jump_horizontal_speed,
                            self.tuning.wall_jump_speed,
                        ];
                        self.variable_jump_timer = self.tuning.variable_jump_seconds;
                    } else if self.abilities.contains(AbilityFlags::DOUBLE_JUMP)
                        && self.air_jumps_remaining > 0
                    {
                        self.velocity[1] = self.tuning.jump_speed;
                        self.air_jumps_remaining -= 1;
                        self.jump_buffer_timer = 0.0;
                        self.variable_jump_timer = self.tuning.variable_jump_seconds;
                    }
                }

                // 起跳保持阶段减半重力；松开跳跃立即截断上升速度。
                let holding_jump = input.jump_held && self.velocity[1] > 0.0;
                if !input.jump_held && self.velocity[1] > 0.0 {
                    self.velocity[1] = self.velocity[1].min(self.tuning.jump_speed * 0.45);
                    self.variable_jump_timer = 0.0;
                }
                let near_apex = self.velocity[1].abs() < 20.0;
                let gravity_scale = if (holding_jump && self.variable_jump_timer > 0.0) || near_apex
                {
                    0.5
                } else {
                    1.0
                };
                self.velocity[1] -= self.tuning.gravity * gravity_scale * dt;
                self.variable_jump_timer = (self.variable_jump_timer - dt).max(0.0);
                if self.velocity[1] < -self.tuning.max_fall_speed {
                    self.velocity[1] = -self.tuning.max_fall_speed;
                }
            }
        } else {
            self.dash_timer -= dt;
        }

        let was_grounded = self.grounded;
        self.grounded = false;
        events.hit_wall = self.apply_movement_result(move_body(
            self.position,
            self.tuning.player_half_size,
            [self.velocity[0] * dt, self.velocity[1] * dt],
            MoveOptions2D {
                drop_through_one_way,
            },
        ));
        if self.grounded && !was_grounded {
            events.landed = true;
        }
        if self.phase == Phase::Dash && self.dash_timer <= 0.0 {
            self.phase = Phase::Normal;
            let length =
                (self.velocity[0] * self.velocity[0] + self.velocity[1] * self.velocity[1]).sqrt();
            if length > self.tuning.dash_end_speed {
                let scale = self.tuning.dash_end_speed / length;
                self.velocity[0] *= scale;
                self.velocity[1] *= scale;
            }
        }

        if overlaps_hazard(self.body()) {
            events.died = self.die();
        }
        events
    }

    fn die(&mut self) -> bool {
        if self.phase == Phase::Dead {
            return false;
        }
        self.phase = Phase::Dead;
        self.respawn_timer = self.tuning.respawn_seconds;
        self.velocity = [0.0, 0.0];
        self.deaths = self.deaths.saturating_add(1);
        true
    }

    fn body(&self) -> Aabb {
        Aabb::new(
            self.position[0] - self.tuning.player_half_size[0],
            self.position[1] - self.tuning.player_half_size[1],
            self.position[0] + self.tuning.player_half_size[0],
            self.position[1] + self.tuning.player_half_size[1],
        )
    }

    fn near_wall<R>(&self, raycast: &mut R) -> f32
    where
        R: FnMut([f32; 2], [f32; 2], f32) -> Option<f32>,
    {
        let body = self.body();
        let center_y = (body.min[1] + body.max[1]) * 0.5;
        if raycast([body.min[0], center_y], [-1.0, 0.0], 1.5).is_some() {
            return -1.0;
        }
        if raycast([body.max[0], center_y], [1.0, 0.0], 1.5).is_some() {
            return 1.0;
        }
        0.0
    }

    fn active_solids(&self) -> impl Iterator<Item = Aabb> + '_ {
        self.geometry.solids.iter().copied().chain(
            (!self.abilities.contains(AbilityFlags::DASH)).then_some(self.geometry.dash_gate),
        )
    }

    fn active_obstacles(&self) -> impl Iterator<Item = Obstacle2D> + '_ {
        self.active_solids().map(Obstacle2D::solid).chain(
            self.geometry
                .one_way_platforms
                .iter()
                .copied()
                .map(Obstacle2D::one_way_top),
        )
    }

    fn apply_movement_result(&mut self, result: MoveResult2) -> bool {
        self.position = result.position;
        self.grounded = result.grounded;
        if result.hit_x {
            self.velocity[0] = 0.0;
        }
        if result.hit_y {
            self.velocity[1] = 0.0;
        }
        result.hit_x
    }
}

fn approach(value: f32, target: f32, amount: f32) -> f32 {
    if (target - value).abs() <= amount {
        target
    } else {
        value + (target - value).signum() * amount
    }
}

fn raycast_solids(
    origin: [f32; 2],
    direction: [f32; 2],
    max_distance: f32,
    solids: &[Aabb],
) -> Option<f32> {
    solids
        .iter()
        .filter_map(|solid| raycast_aabb(origin, direction, max_distance, *solid))
        .map(|hit| hit.distance)
        .min_by(f32::total_cmp)
}

fn resolve_enemy_contacts(
    world: &mut GameWorld<'_>,
    bindings: &PlatformerBindings,
    player: &mut PlayerState,
    dash_attack_active: bool,
) -> (bool, bool) {
    if player.phase == Phase::Dead {
        return (false, false);
    }
    let mut enemy_defeated = false;
    if dash_attack_active {
        for contact in world.damage_contacts2d(DASH_HITBOX_LAYER, ENEMY_LAYER) {
            if Some(contact.hitbox) != bindings.player_hitbox {
                continue;
            }
            let Some(index) = bindings
                .enemies
                .iter()
                .position(|&entity| entity == contact.hurtbox)
            else {
                continue;
            };
            let Some(enemy) = player.enemies.get_mut(index) else {
                continue;
            };
            if !enemy.alive {
                continue;
            }
            enemy.health = enemy.health.saturating_sub(contact.damage);
            if enemy.health == 0 {
                enemy.alive = false;
                enemy_defeated = true;
                world.set(contact.hurtbox, Active(false));
                world.set(
                    contact.hurtbox,
                    Hurtbox2D {
                        team: ENEMY_TEAM,
                        enabled: false,
                    },
                );
            }
            // Preserve forward momentum while pushing the player clear of the
            // hurtbox so one dash cannot repeatedly hit the same enemy.
            let away = if player.position[0] <= enemy.position[0] {
                -1.0
            } else {
                1.0
            };
            player.position[0] +=
                away * (enemy.definition.half_size[0] + player.tuning.player_half_size[0] + 1.0);
            player.velocity[0] = away * player.velocity[0].abs().max(80.0);
        }
    }

    if !dash_attack_active {
        for contact in world.overlap2d(player.body(), ENEMY_LAYER) {
            let Some(index) = bindings
                .enemies
                .iter()
                .position(|&entity| entity == contact.entity)
            else {
                continue;
            };
            if player.enemies.get(index).is_some_and(|enemy| enemy.alive) {
                return (player.die(), enemy_defeated);
            }
        }
    }
    (false, enemy_defeated)
}

#[derive(Default)]
pub struct PlatformerBindings {
    pub player: Option<EntityHandle>,
    pub camera: Option<EntityHandle>,
    pub hud: Option<EntityHandle>,
    pub debug: Option<EntityHandle>,
    pub crystal: Option<EntityHandle>,
    pub player_hitbox: Option<EntityHandle>,
    pub tilemap: Option<EntityHandle>,
    pub gate: Option<EntityHandle>,
    pub spikes: Vec<EntityHandle>,
    pub enemies: Vec<EntityHandle>,
    pub exit_triggers: Vec<EntityHandle>,
}

impl PlatformerBindings {
    fn discover(world: &GameWorld<'_>) -> Self {
        let mut bindings = Self::default();
        for entity in world.query::<Name>() {
            let Some(name) = world.get::<Name>(entity) else {
                continue;
            };
            match name.0.as_str() {
                "player" => bindings.player = Some(entity),
                "camera" => bindings.camera = Some(entity),
                "hud" => bindings.hud = Some(entity),
                "debug" => bindings.debug = Some(entity),
                "crystal" => bindings.crystal = Some(entity),
                "player-dash-hitbox" => bindings.player_hitbox = Some(entity),
                "tilemap" => bindings.tilemap = Some(entity),
                "dash-gate" => bindings.gate = Some(entity),
                name if name.starts_with("spike-") => bindings.spikes.push(entity),
                name if name.starts_with("enemy-") => bindings.enemies.push(entity),
                name if name.starts_with("room-exit-") => bindings.exit_triggers.push(entity),
                _ => {}
            }
        }
        bindings.spikes.sort_by_key(|entity| {
            world
                .get::<Name>(*entity)
                .and_then(|name| {
                    name.0
                        .strip_prefix("spike-")
                        .and_then(|index| index.parse::<usize>().ok())
                })
                .unwrap_or(usize::MAX)
        });
        bindings.enemies.sort_by_key(|entity| {
            world
                .get::<Name>(*entity)
                .and_then(|name| {
                    name.0
                        .strip_prefix("enemy-")
                        .and_then(|index| index.parse::<usize>().ok())
                })
                .unwrap_or(usize::MAX)
        });
        bindings.exit_triggers.sort_by_key(|entity| {
            world
                .get::<Name>(*entity)
                .and_then(|name| {
                    name.0
                        .strip_prefix("room-exit-")
                        .and_then(|index| index.parse::<usize>().ok())
                })
                .unwrap_or(usize::MAX)
        });
        bindings
    }
}

fn spawn_room(
    world: &mut GameWorld<'_>,
    geometry: &WorldGeometry,
    bindings: &mut PlatformerBindings,
) {
    let player = spawn_sprite(
        world,
        "player",
        geometry.spawn,
        "sprites/hero.png",
        [0.7, 0.9, 1.0, 1.0],
        20,
    );
    bindings.player = Some(player);
    world.set(
        player,
        Hurtbox2D {
            team: PLAYER_TEAM,
            enabled: true,
        },
    );
    if let Some(mut sprite) = world.get::<SpriteRenderer>(player) {
        sprite.frame_size = [16, 24];
        sprite.sheet_columns = 4;
        // Physics stores the player position at the body centre. Keep the
        // sprite on the same centre and scale the source frame to the body
        // bounds instead of compensating with a foot pivot.
        sprite.pivot = [0.5, 0.5];
        world.set(player, sprite);
    }
    resize_sprite_from_source(
        world,
        player,
        2.0 * Tuning::default().player_half_size[0],
        2.0 * Tuning::default().player_half_size[1],
        [16.0, 24.0],
    );
    world.set(
        player,
        SpriteAnimation {
            frames: 4,
            fps: 8.0,
            ..SpriteAnimation::default()
        },
    );

    let tilemap = world.spawn();
    world.set(tilemap, Name("tilemap".to_string()));
    world.set(tilemap, LocalTransform::default());
    world.set(
        tilemap,
        Tilemap {
            tileset_path: "sprites/tileset.png".to_string(),
            tile_size: [16, 16],
            cell_size: [16.0, 16.0],
            columns: geometry.columns,
            tiles: geometry.tiles.clone(),
            color: [0.25, 0.32, 0.55, 1.0],
            sorting_layer: -10,
            ..Tilemap::default()
        },
    );
    world.set(
        tilemap,
        TilemapCollider2D {
            solid_tile_ids: vec![1, 2, 3],
            one_way_tile_ids: geometry.one_way_tile_ids.clone(),
            layer: SOLID_LAYER,
            ..TilemapCollider2D::default()
        },
    );
    world.set(tilemap, Active(true));
    bindings.tilemap = Some(tilemap);

    for (index, hazard) in geometry.hazards.iter().copied().enumerate() {
        let position = [
            (hazard.min[0] + hazard.max[0]) * 0.5,
            (hazard.min[1] + hazard.max[1]) * 0.5,
        ];
        let spike = spawn_sprite(
            world,
            &format!("spike-{index}"),
            position,
            "sprites/spikes.png",
            [1.0, 0.3, 0.38, 1.0],
            10,
        );
        resize_sprite(
            world,
            spike,
            hazard.max[0] - hazard.min[0],
            hazard.max[1] - hazard.min[1],
        );
        set_box_collider(world, spike, hazard, true, HAZARD_LAYER);
        bindings.spikes.push(spike);
    }

    for (index, enemy) in geometry.enemies.iter().enumerate() {
        let entity = spawn_sprite(
            world,
            &format!("enemy-{index}"),
            enemy.spawn,
            "sprites/lamp.png",
            [1.0, 0.48, 0.38, 1.0],
            12,
        );
        resize_sprite_from_source(
            world,
            entity,
            enemy.half_size[0] * 2.0,
            enemy.half_size[1] * 2.0,
            [12.0, 16.0],
        );
        set_box_collider(
            world,
            entity,
            Aabb::from_center_half_extents(enemy.spawn, enemy.half_size),
            true,
            ENEMY_LAYER,
        );
        world.set(
            entity,
            Hurtbox2D {
                team: ENEMY_TEAM,
                enabled: true,
            },
        );
        bindings.enemies.push(entity);
    }

    let gate_position = [
        (geometry.dash_gate.min[0] + geometry.dash_gate.max[0]) * 0.5,
        (geometry.dash_gate.min[1] + geometry.dash_gate.max[1]) * 0.5,
    ];
    let gate = spawn_sprite(
        world,
        "dash-gate",
        gate_position,
        "sprites/door.png",
        [0.9, 0.35, 0.95, 0.8],
        5,
    );
    resize_sprite_from_source(
        world,
        gate,
        geometry.dash_gate.max[0] - geometry.dash_gate.min[0],
        geometry.dash_gate.max[1] - geometry.dash_gate.min[1],
        [32.0, 48.0],
    );
    set_box_collider(world, gate, geometry.dash_gate, false, SOLID_LAYER);
    bindings.gate = Some(gate);

    let crystal = spawn_sprite(
        world,
        "crystal",
        geometry.crystal,
        "sprites/crystal.png",
        [0.35, 0.9, 1.0, 1.0],
        15,
    );
    // The pickup AABB is 16x16 and is centred on `geometry.crystal`.
    // The source art is 16x20, so fit the quad to the gameplay bounds.
    resize_sprite_from_source(world, crystal, 16.0, 16.0, [16.0, 20.0]);
    set_box_collider(
        world,
        crystal,
        Aabb::from_center_half_extents(geometry.crystal, [8.0, 8.0]),
        true,
        PICKUP_LAYER,
    );
    bindings.crystal = Some(crystal);

    let dust = world.spawn();
    world.set(dust, Name("dust".to_string()));
    world.set(
        dust,
        LocalTransform {
            position: glam::Vec3::new(160.0, 17.0, 0.0),
            ..LocalTransform::default()
        },
    );
    world.set(
        dust,
        ParticleEmitter2D {
            emission_rate: 12.0,
            lifetime: [0.25, 0.6],
            start_speed: [8.0, 18.0],
            start_size: [1.0, 2.0],
            start_color: [0.55, 0.7, 1.0, 0.55],
            end_color: [0.2, 0.35, 0.8, 0.0],
            gravity: [0.0, -18.0],
            direction: [0.0, 1.0],
            spread_degrees: 80.0,
            sorting_layer: 30,
            ..ParticleEmitter2D::default()
        },
    );
    world.set(dust, Active(true));

    let light = world.spawn();
    world.set(light, Name("well-light".to_string()));
    world.set(
        light,
        LocalTransform {
            position: glam::Vec3::new(160.0, 100.0, 1.0),
            ..LocalTransform::default()
        },
    );
    world.set(
        light,
        Light2D {
            kind: Light2DKind::Ambient,
            color: [0.35, 0.42, 0.8],
            intensity: 0.8,
            radius: 0.0,
            ..Light2D::default()
        },
    );
    world.set(light, Active(true));

    bindings.hud = Some(spawn_text(
        world,
        "hud",
        "ECHO WELL   DASH LOCKED",
        18.0,
        8.0,
    ));
    bindings.debug = Some(spawn_text(world, "debug", "", 12.0, 30.0));
}

fn spawn_sprite(
    world: &mut GameWorld<'_>,
    name: &str,
    position: [f32; 2],
    path: &str,
    color: [f32; 4],
    sorting_layer: i32,
) -> EntityHandle {
    let entity = world.spawn();
    world.set(entity, Name(name.to_string()));
    world.set(
        entity,
        LocalTransform {
            position: glam::Vec3::new(position[0], position[1], 0.0),
            ..LocalTransform::default()
        },
    );
    world.set(
        entity,
        SpriteRenderer {
            sprite_path: path.to_string(),
            pixels_per_unit: 1.0,
            pivot: [0.5, 0.5],
            color,
            sorting_layer,
            receive_lights: true,
            ..SpriteRenderer::default()
        },
    );
    world.set(entity, Active(true));
    let _ = world.enable_transform_interpolation2d(entity);
    entity
}

#[cfg(test)]
fn spawn_solid_collider(world: &mut GameWorld<'_>, index: usize, bounds: Aabb) -> EntityHandle {
    let entity = world.spawn();
    world.set(entity, Name(format!("solid-{index}")));
    world.set(
        entity,
        LocalTransform {
            position: glam::Vec3::new(
                (bounds.min[0] + bounds.max[0]) * 0.5,
                (bounds.min[1] + bounds.max[1]) * 0.5,
                0.0,
            ),
            ..LocalTransform::default()
        },
    );
    world.set(
        entity,
        Collider2D {
            half_extents: [
                (bounds.max[0] - bounds.min[0]) * 0.5,
                (bounds.max[1] - bounds.min[1]) * 0.5,
            ],
            layer: SOLID_LAYER,
            ..Collider2D::default()
        },
    );
    world.set(entity, Active(true));
    entity
}

fn set_box_collider(
    world: &mut GameWorld<'_>,
    entity: EntityHandle,
    bounds: Aabb,
    is_trigger: bool,
    layer: u32,
) {
    let center = [
        (bounds.min[0] + bounds.max[0]) * 0.5,
        (bounds.min[1] + bounds.max[1]) * 0.5,
    ];
    set_sprite_center(world, entity, center);
    let scale = world
        .get::<LocalTransform>(entity)
        .map(|transform| [transform.scale.x.abs(), transform.scale.y.abs()])
        .unwrap_or([1.0; 2]);
    world.set(
        entity,
        Collider2D {
            offset: [0.0; 2],
            half_extents: [
                ((bounds.max[0] - bounds.min[0]) * 0.5) / scale[0].max(0.0001),
                ((bounds.max[1] - bounds.min[1]) * 0.5) / scale[1].max(0.0001),
            ],
            is_trigger,
            layer,
            one_way_top: false,
            ..Collider2D::default()
        },
    );
}

fn resize_sprite(world: &mut GameWorld<'_>, entity: EntityHandle, width: f32, height: f32) {
    resize_sprite_from_source(world, entity, width, height, [16.0, 16.0]);
}

fn resize_sprite_from_source(
    world: &mut GameWorld<'_>,
    entity: EntityHandle,
    width: f32,
    height: f32,
    source_size: [f32; 2],
) {
    if let Some(mut transform) = world.get::<LocalTransform>(entity) {
        transform.scale = glam::Vec3::new(
            width / source_size[0].max(1.0),
            height / source_size[1].max(1.0),
            1.0,
        );
        world.set(entity, transform);
    }
}

/// Keep every gameplay sprite on the same centre and dimensions as its
/// corresponding gameplay AABB. This also runs for entities authored in a
/// scene, so scene replacement cannot reintroduce a pivot/scale mismatch.
fn align_collision_sprites(
    world: &mut GameWorld<'_>,
    geometry: &WorldGeometry,
    bindings: &PlatformerBindings,
    tuning: &Tuning,
    player_center: [f32; 2],
) {
    if let Some(player) = bindings.player {
        set_sprite_center(world, player, player_center);
        if let Some(mut sprite) = world.get::<SpriteRenderer>(player) {
            sprite.frame_size = [16, 24];
            sprite.sheet_columns = 4;
            sprite.pivot = [0.5, 0.5];
            world.set(player, sprite);
        }
        resize_sprite_from_source(
            world,
            player,
            tuning.player_half_size[0] * 2.0,
            tuning.player_half_size[1] * 2.0,
            [16.0, 24.0],
        );
        set_box_collider(
            world,
            player,
            Aabb::from_center_half_extents(player_center, tuning.player_half_size),
            false,
            SOLID_LAYER,
        );
        world.set(
            player,
            Hurtbox2D {
                team: PLAYER_TEAM,
                enabled: true,
            },
        );
        let _ = world.snap_transform_interpolation2d(player);
    }

    for (spike, hazard) in bindings.spikes.iter().copied().zip(geometry.hazards.iter()) {
        set_sprite_center(
            world,
            spike,
            [
                (hazard.min[0] + hazard.max[0]) * 0.5,
                (hazard.min[1] + hazard.max[1]) * 0.5,
            ],
        );
        if let Some(mut sprite) = world.get::<SpriteRenderer>(spike) {
            sprite.pivot = [0.5, 0.5];
            world.set(spike, sprite);
        }
        resize_sprite_from_source(
            world,
            spike,
            hazard.max[0] - hazard.min[0],
            hazard.max[1] - hazard.min[1],
            [16.0, 16.0],
        );
        set_box_collider(world, spike, *hazard, true, HAZARD_LAYER);
        let _ = world.snap_transform_interpolation2d(spike);
    }

    if let Some(gate) = bindings.gate {
        set_sprite_center(
            world,
            gate,
            [
                (geometry.dash_gate.min[0] + geometry.dash_gate.max[0]) * 0.5,
                (geometry.dash_gate.min[1] + geometry.dash_gate.max[1]) * 0.5,
            ],
        );
        if let Some(mut sprite) = world.get::<SpriteRenderer>(gate) {
            sprite.pivot = [0.5, 0.5];
            world.set(gate, sprite);
        }
        resize_sprite_from_source(
            world,
            gate,
            geometry.dash_gate.max[0] - geometry.dash_gate.min[0],
            geometry.dash_gate.max[1] - geometry.dash_gate.min[1],
            [32.0, 48.0],
        );
        set_box_collider(world, gate, geometry.dash_gate, false, SOLID_LAYER);
        let _ = world.snap_transform_interpolation2d(gate);
    }

    if let Some(crystal) = bindings.crystal {
        set_sprite_center(world, crystal, geometry.crystal);
        if let Some(mut sprite) = world.get::<SpriteRenderer>(crystal) {
            sprite.pivot = [0.5, 0.5];
            world.set(crystal, sprite);
        }
        resize_sprite_from_source(world, crystal, 16.0, 16.0, [16.0, 20.0]);
        set_box_collider(
            world,
            crystal,
            Aabb::from_center_half_extents(geometry.crystal, [8.0, 8.0]),
            true,
            PICKUP_LAYER,
        );
        let _ = world.snap_transform_interpolation2d(crystal);
    }
}

fn sync_room_visuals(
    world: &mut GameWorld<'_>,
    geometry: &WorldGeometry,
    exits: &[RoomExit],
    bindings: &mut PlatformerBindings,
    tuning: &Tuning,
    player_center: [f32; 2],
) {
    if let Some(tilemap) = bindings.tilemap {
        if let Some(mut tiles) = world.get::<Tilemap>(tilemap) {
            tiles.columns = geometry.columns;
            tiles.tiles = geometry.tiles.clone();
            world.set(tilemap, tiles);
        }
    }

    while bindings.spikes.len() > geometry.hazards.len() {
        if let Some(spike) = bindings.spikes.pop() {
            world.despawn(spike);
        }
    }
    while bindings.spikes.len() < geometry.hazards.len() {
        let index = bindings.spikes.len();
        let hazard = geometry.hazards[index];
        let position = [
            (hazard.min[0] + hazard.max[0]) * 0.5,
            (hazard.min[1] + hazard.max[1]) * 0.5,
        ];
        let spike = spawn_sprite(
            world,
            &format!("spike-{index}"),
            position,
            "sprites/spikes.png",
            [1.0, 0.3, 0.38, 1.0],
            10,
        );
        bindings.spikes.push(spike);
    }

    while bindings.enemies.len() > geometry.enemies.len() {
        if let Some(enemy) = bindings.enemies.pop() {
            world.despawn(enemy);
        }
    }
    while bindings.enemies.len() < geometry.enemies.len() {
        let index = bindings.enemies.len();
        let definition = &geometry.enemies[index];
        let enemy = spawn_sprite(
            world,
            &format!("enemy-{index}"),
            definition.spawn,
            "sprites/lamp.png",
            [1.0, 0.48, 0.38, 1.0],
            12,
        );
        resize_sprite_from_source(
            world,
            enemy,
            definition.half_size[0] * 2.0,
            definition.half_size[1] * 2.0,
            [12.0, 16.0],
        );
        set_box_collider(
            world,
            enemy,
            definition_bounds(definition),
            true,
            ENEMY_LAYER,
        );
        world.set(
            enemy,
            Hurtbox2D {
                team: ENEMY_TEAM,
                enabled: true,
            },
        );
        bindings.enemies.push(enemy);
    }

    while bindings.exit_triggers.len() > exits.len() {
        if let Some(trigger) = bindings.exit_triggers.pop() {
            world.despawn(trigger);
        }
    }
    while bindings.exit_triggers.len() < exits.len() {
        let trigger = world.spawn();
        world.set(trigger, LocalTransform::default());
        world.set(trigger, Active(true));
        bindings.exit_triggers.push(trigger);
    }
    for (index, (trigger, exit)) in bindings
        .exit_triggers
        .iter()
        .copied()
        .zip(exits)
        .enumerate()
    {
        world.set(trigger, Name(format!("room-exit-{index}")));
        set_box_collider(world, trigger, exit.trigger, true, EXIT_LAYER);
        world.set(trigger, Active(true));
    }

    if let Some(player) = bindings.player {
        world.set(
            player,
            Hurtbox2D {
                team: PLAYER_TEAM,
                enabled: true,
            },
        );
        ensure_player_hitbox(world, bindings);
    }

    align_collision_sprites(world, geometry, bindings, tuning, player_center);
}

fn ensure_player_hitbox(world: &mut GameWorld<'_>, bindings: &mut PlatformerBindings) {
    let hitbox = match bindings.player_hitbox {
        Some(hitbox) => hitbox,
        None => {
            let hitbox = world.spawn();
            bindings.player_hitbox = Some(hitbox);
            hitbox
        }
    };
    world.set(hitbox, Name("player-dash-hitbox".to_string()));
    if world.get::<LocalTransform>(hitbox).is_none() {
        world.set(hitbox, LocalTransform::default());
    }
    world.set(
        hitbox,
        Collider2D {
            half_extents: DASH_HITBOX_HALF_EXTENTS,
            is_trigger: true,
            layer: DASH_HITBOX_LAYER,
            ..Collider2D::default()
        },
    );
    world.set(
        hitbox,
        Hitbox2D {
            damage: 1,
            team: PLAYER_TEAM,
            enabled: false,
        },
    );
    world.set(hitbox, Active(true));
}

fn definition_bounds(definition: &EnemyDefinition) -> Aabb {
    Aabb::from_center_half_extents(definition.spawn, definition.half_size)
}

fn set_sprite_center(world: &mut GameWorld<'_>, entity: EntityHandle, center: [f32; 2]) {
    if let Some(mut transform) = world.get::<LocalTransform>(entity) {
        transform.position.x = center[0];
        transform.position.y = center[1];
        world.set(entity, transform);
    }
}

fn sync_player_transform(world: &mut GameWorld<'_>, player: EntityHandle, position: [f32; 2]) {
    if let Some(mut transform) = world.get::<LocalTransform>(player) {
        transform.position.x = position[0];
        transform.position.y = position[1];
        world.set(player, transform);
    }
}

fn sync_player_hitbox(
    world: &mut GameWorld<'_>,
    hitbox: EntityHandle,
    player: &PlayerState,
    enabled: bool,
) {
    let direction = if player.velocity[0].abs() > 0.01 {
        player.velocity[0].signum()
    } else {
        player.facing
    };
    let center = [
        player.position[0]
            + direction * (player.tuning.player_half_size[0] + DASH_HITBOX_HALF_EXTENTS[0] - 2.0),
        player.position[1],
    ];
    if let Some(mut transform) = world.get::<LocalTransform>(hitbox) {
        transform.position.x = center[0];
        transform.position.y = center[1];
        world.set(hitbox, transform);
    }
    world.set(
        hitbox,
        Hitbox2D {
            damage: 1,
            team: PLAYER_TEAM,
            enabled,
        },
    );
}

fn sync_enemy_visuals(
    world: &mut GameWorld<'_>,
    bindings: &PlatformerBindings,
    enemies: &[EnemyState],
) {
    for (entity, enemy) in bindings.enemies.iter().copied().zip(enemies) {
        set_sprite_center(world, entity, enemy.position);
        world.set(entity, Active(enemy.alive));
        world.set(
            entity,
            Hurtbox2D {
                team: ENEMY_TEAM,
                enabled: enemy.alive,
            },
        );
        if let Some(mut sprite) = world.get::<SpriteRenderer>(entity) {
            sprite.flip_x = enemy.direction < 0.0;
            sprite.color = if enemy.alive {
                [1.0, 0.48, 0.38, 1.0]
            } else {
                [1.0, 0.25, 0.2, 0.0]
            };
            world.set(entity, sprite);
        }
    }
}

fn spawn_text(
    world: &mut GameWorld<'_>,
    name: &str,
    content: &str,
    font_size: f32,
    top: f32,
) -> EntityHandle {
    let entity = world.spawn();
    world.set(entity, Name(name.to_string()));
    world.set(entity, Node::default());
    world.set(
        entity,
        Canvas {
            reference_resolution: [1280.0, 720.0],
            scale_mode: CanvasScaleMode::ScaleWithScreenSize,
            match_width_or_height: 0.5,
        },
    );
    world.set(
        entity,
        Style {
            width: Some(300.0),
            height: Some(font_size + 8.0),
            anchors: Anchors::TOP_LEFT,
            pivot: Pivot::TOP_LEFT,
            margin: Margin {
                left: 8.0,
                top,
                ..Margin::default()
            },
            ..Style::default()
        },
    );
    world.set(
        entity,
        Text {
            content: content.to_string(),
            font_size,
            color: [0.8, 0.9, 1.0, 1.0],
            alignment: TextAlign::Left,
            ..Text::default()
        },
    );
    world.set(entity, Active(true));
    entity
}

pub struct PlatformerRuntime {
    pub player: PlayerState,
    pub bindings: PlatformerBindings,
    pub geometry: WorldGeometry,
    rooms: Vec<RoomDefinition>,
    current_room: usize,
    camera_bounds: [f32; 4],
    tuning_reload_timer: f32,
    pending_events: StepEvents,
    input_actions: InputActionMap,
    pub debug_enabled: bool,
}

/// ECS resource 名称，便于项目入口和 preview 模块保持简洁。
pub type PlatformerState = PlatformerRuntime;

impl PlatformerRuntime {
    fn build(world: &mut GameWorld<'_>) -> Self {
        let rooms = load_rooms(world);
        let saved_room = world.load_data("echowell.room");
        let current_room = saved_room
            .as_deref()
            .and_then(|room_id| rooms.iter().position(|room| room.id == room_id))
            .unwrap_or(0);
        let room = &rooms[current_room];
        let geometry = room.geometry.clone();
        let tuning = world
            .read_asset_text("player.json")
            .ok()
            .and_then(|text| serde_json::from_str::<Tuning>(&text).ok())
            .unwrap_or_default();
        let mut player = PlayerState::new_in_room(&room.id, tuning, geometry.clone());
        let legacy_abilities = world
            .load_data("echowell.abilities")
            .and_then(|value| value.parse::<u32>().ok());
        let abilities_migrated = world.get_flag("echowell.save.abilities_migrated");
        for ability in [
            AbilityFlags::WALL_JUMP,
            AbilityFlags::DASH,
            AbilityFlags::DOUBLE_JUMP,
        ] {
            let name = ability_flag_name(ability);
            let legacy_enabled = legacy_abilities.is_some_and(|bits| bits & ability != 0);
            let enabled = world.get_flag(name) || (!abilities_migrated && legacy_enabled);
            if enabled {
                player.unlock(ability);
            }
            if !abilities_migrated && legacy_abilities.is_some() {
                let _ = world.set_flag(name, legacy_enabled);
            }
        }
        if !abilities_migrated {
            let _ = world.set_flag("echowell.save.abilities_migrated", true);
        }
        if let Some(deaths) = world
            .load_data("echowell.deaths")
            .and_then(|value| value.parse::<u32>().ok())
        {
            player.deaths = deaths;
        }
        let mut bindings = PlatformerBindings::discover(world);
        if bindings.player.is_none() && bindings.camera.is_some() {
            spawn_room(world, &geometry, &mut bindings);
        }
        sync_room_visuals(
            world,
            &geometry,
            &room.exits,
            &mut bindings,
            &player.tuning,
            player.position,
        );
        sync_enemy_visuals(world, &bindings, &player.enemies);
        let camera_bounds = room.bounds;
        let runtime = Self {
            player,
            bindings,
            geometry,
            camera_bounds,
            rooms,
            current_room,
            tuning_reload_timer: 0.0,
            pending_events: StepEvents::default(),
            input_actions: platformer_input_map(),
            debug_enabled: false,
        };
        runtime.sync_camera_config(world);
        runtime
    }

    fn sync_camera_config(&self, world: &mut GameWorld<'_>) {
        let Some(camera) = self.bindings.camera else {
            return;
        };
        let Some(player) = self.bindings.player else {
            return;
        };
        let mut follow = world.get::<Camera2D>(camera).unwrap_or_default();
        follow.enabled = true;
        follow.target_entity = ((player.generation as u64) << 32) | player.id as u64;
        follow.damping = 8.0;
        follow.deadzone = [4.0, 2.0];
        follow.confiner_enabled = true;
        follow.confiner_bounds = self.camera_bounds;
        follow.pixel_snapping = false;
        world.set(camera, follow);
    }

    fn transition_room(&mut self, world: &mut GameWorld<'_>) -> bool {
        if !can_transition_from_phase(self.player.phase) {
            return false;
        }
        let Some(player) = self.bindings.player else {
            return false;
        };
        let trigger_events = world.trigger_events2d();
        let Some(exit) = self.rooms.get(self.current_room).and_then(|room| {
            room.exits
                .iter()
                .zip(&self.bindings.exit_triggers)
                .find(|(exit, trigger)| {
                    exit.required_ability
                        .is_none_or(|ability| self.player.abilities.contains(ability))
                        && trigger_events.iter().any(|event| {
                            event.phase != TriggerPhase2D::Exit
                                && ((event.entity_a == player && event.entity_b == **trigger)
                                    || (event.entity_b == player && event.entity_a == **trigger))
                        })
                })
                .map(|(exit, _)| exit)
        }) else {
            return false;
        };
        let Some(next) = self.rooms.iter().position(|room| room.id == exit.target) else {
            return false;
        };
        let entry = exit.entry;
        let room = self.rooms[next].clone();
        self.current_room = next;
        self.camera_bounds = room.bounds;
        self.geometry = room.geometry.clone();
        enter_room(&mut self.player, &room, entry);
        sync_room_visuals(
            world,
            &self.geometry,
            &room.exits,
            &mut self.bindings,
            &self.player.tuning,
            entry,
        );
        self.sync_camera_config(world);
        sync_enemy_visuals(world, &self.bindings, &self.player.enemies);
        if let Some(player) = self.bindings.player {
            let _ = world.snap_transform_interpolation2d(player);
        }
        for entity in self.bindings.enemies.iter().copied() {
            let _ = world.snap_transform_interpolation2d(entity);
        }
        let _ = world.save_data("echowell.room", &room.id);
        true
    }

    fn sync_replay_room(&mut self, world: &mut GameWorld<'_>) {
        let Some(index) = self
            .rooms
            .iter()
            .position(|room| room.id == self.player.room_id)
        else {
            return;
        };
        if index == self.current_room {
            return;
        }
        self.current_room = index;
        let room = self.rooms[index].clone();
        self.camera_bounds = room.bounds;
        self.geometry = room.geometry.clone();
        self.player.geometry = self.geometry.clone();
        sync_room_visuals(
            world,
            &self.geometry,
            &room.exits,
            &mut self.bindings,
            &self.player.tuning,
            self.player.position,
        );
        self.sync_camera_config(world);
        sync_enemy_visuals(world, &self.bindings, &self.player.enemies);
        if let Some(player) = self.bindings.player {
            let _ = world.snap_transform_interpolation2d(player);
        }
        for entity in self.bindings.enemies.iter().copied() {
            let _ = world.snap_transform_interpolation2d(entity);
        }
    }

    fn write_back(&mut self, world: &mut GameWorld<'_>, events: StepEvents, _render_alpha: f32) {
        let dash_unlocked = self.player.abilities.contains(AbilityFlags::DASH)
            || world.get_flag(ability_flag_name(AbilityFlags::DASH));
        if let Some(player) = self.bindings.player {
            if let Some(mut transform) = world.get::<LocalTransform>(player) {
                // Keep simulation coordinates on the collision-box centre.
                transform.position.x = self.player.position[0];
                transform.position.y = self.player.position[1];
                transform.position.z = 0.0;
                // Keep hot-reloaded player dimensions aligned with the
                // 16x24 source frame used by the sprite sheet.
                transform.scale.x = (self.player.tuning.player_half_size[0] * 2.0) / 16.0;
                transform.scale.y = (self.player.tuning.player_half_size[1] * 2.0) / 24.0;
                world.set(player, transform);
            }
            if let Some(mut sprite) = world.get::<SpriteRenderer>(player) {
                sprite.flip_x = self.player.facing < 0.0;
                sprite.color = if self.player.phase == Phase::Dead {
                    [1.0, 0.35, 0.35, 0.45]
                } else {
                    [1.0, 1.0, 1.0, 1.0]
                };
                world.set(player, sprite);
            }
        }
        if let Some(hud) = self.bindings.hud {
            if let Some(mut text) = world.get::<Text>(hud) {
                text.content = format!(
                    "ECHO WELL   DASH {}   DEATHS {}",
                    if dash_unlocked { "READY" } else { "LOCKED" },
                    self.player.deaths
                );
                world.set(hud, text);
            }
        }
        if let Some(debug) = self.bindings.debug {
            world.set(debug, Active(self.debug_enabled));
            if let Some(mut text) = world.get::<Text>(debug) {
                text.content = self.player.debug_line();
                world.set(debug, text);
            }
        }
        if let Some(crystal) = self.bindings.crystal {
            world.set(crystal, Active(!dash_unlocked));
        }
        if let Some(gate) = self.bindings.gate {
            world.set(gate, Active(!dash_unlocked));
        }
        if events.ability_unlocked {
            let _ = world.play_sound("audio/crystal.wav", 0.8, false);
        }
        if events.died {
            let _ = world.save_data("echowell.deaths", &self.player.deaths.to_string());
            let _ = world.play_sound("audio/death.wav", 0.7, false);
        }
        if events.respawned {
            let _ = world.play_sound("audio/respawn.wav", 0.5, false);
        }
    }

    fn collect_crystal(&mut self, world: &mut GameWorld<'_>) -> bool {
        if self.player.abilities.contains(AbilityFlags::DASH)
            || world.get_flag(ability_flag_name(AbilityFlags::DASH))
        {
            return false;
        }
        let touching_crystal = world
            .overlap2d(self.player.body(), PICKUP_LAYER)
            .iter()
            .any(|collider| Some(collider.entity) == self.bindings.crystal);
        if !touching_crystal {
            return false;
        }
        self.player.unlock(AbilityFlags::DASH);
        let _ = world.set_flag(ability_flag_name(AbilityFlags::DASH), true);
        let _ = world.save_data(
            "echowell.abilities",
            &self.player.abilities.bits().to_string(),
        );
        true
    }
}

pub fn frame_tick(state: &mut Option<PlatformerRuntime>, world: &mut GameWorld<'_>, dt: f32) {
    if state.is_none() {
        *state = Some(PlatformerRuntime::build(world));
    }
    let Some(runtime) = state.as_mut() else {
        return;
    };
    let snapshot = world.input();
    if snapshot.key_pressed(F1_KEY_INDEX) {
        runtime.debug_enabled = !runtime.debug_enabled;
    }
    if snapshot.key_pressed(keys::KEY_E) {
        if runtime.player.replay.recording {
            runtime.player.replay.recording = false;
        } else {
            runtime.player.begin_recording();
        }
    }
    if snapshot.key_pressed(keys::ENTER) {
        runtime.player.begin_playback();
        runtime.sync_replay_room(world);
        sync_enemy_visuals(world, &runtime.bindings, &runtime.player.enemies);
        if let Some(player) = runtime.bindings.player {
            sync_player_transform(world, player, runtime.player.position);
            let _ = world.snap_transform_interpolation2d(player);
        }
    }
    runtime.tuning_reload_timer -= dt.max(0.0);
    if runtime.tuning_reload_timer <= 0.0 && !runtime.player.replay.playing {
        runtime.tuning_reload_timer = 0.25;
        if let Ok(text) = world.read_asset_text("player.json") {
            if let Ok(tuning) = serde_json::from_str::<Tuning>(&text) {
                runtime.player.tuning = tuning;
            }
        }
    }
    let events = std::mem::take(&mut runtime.pending_events);
    runtime.write_back(world, events, world.interpolation_alpha());
}

pub fn fixed_tick(state: &mut Option<PlatformerRuntime>, world: &mut GameWorld<'_>, dt: f32) {
    if state.is_none() {
        *state = Some(PlatformerRuntime::build(world));
    }
    let Some(runtime) = state.as_mut() else {
        return;
    };

    for ability in [
        AbilityFlags::WALL_JUMP,
        AbilityFlags::DASH,
        AbilityFlags::DOUBLE_JUMP,
    ] {
        if world.get_flag(ability_flag_name(ability)) {
            runtime.player.unlock(ability);
        }
    }
    let input = InputFrame::from_actions(world.input_actions(&mut runtime.input_actions));
    let was_dashing = runtime.player.phase == Phase::Dash;
    let mut events = if let Some(player) = runtime.bindings.player {
        runtime.player.step_with_world(input, dt, world, player)
    } else {
        runtime.player.step(input, dt)
    };
    if events.respawned {
        for enemy in &mut runtime.player.enemies {
            enemy.reset();
        }
    }
    for enemy in &mut runtime.player.enemies {
        enemy.step(dt);
    }
    sync_enemy_visuals(world, &runtime.bindings, &runtime.player.enemies);
    let dash_attack_active = was_dashing || events.dashed || runtime.player.phase == Phase::Dash;
    if let Some(player) = runtime.bindings.player {
        sync_player_transform(world, player, runtime.player.position);
    }
    if let Some(hitbox) = runtime.bindings.player_hitbox {
        sync_player_hitbox(world, hitbox, &runtime.player, dash_attack_active);
    }
    if runtime.player.phase != Phase::Dead {
        let (died, defeated) = resolve_enemy_contacts(
            world,
            &runtime.bindings,
            &mut runtime.player,
            dash_attack_active,
        );
        events.died |= died;
        events.enemy_defeated = defeated;
    }
    if runtime.player.phase != Phase::Dead {
        events.ability_unlocked = runtime.collect_crystal(world);
        runtime.transition_room(world);
    }
    if let Some(player) = runtime.bindings.player {
        sync_player_transform(world, player, runtime.player.position);
        if events.respawned {
            let _ = world.snap_transform_interpolation2d(player);
        }
    }
    if let Some(hitbox) = runtime.bindings.player_hitbox {
        sync_player_hitbox(
            world,
            hitbox,
            &runtime.player,
            runtime.player.phase == Phase::Dash,
        );
    }
    if events.respawned {
        for entity in runtime.bindings.enemies.iter().copied() {
            let _ = world.snap_transform_interpolation2d(entity);
        }
    }
    runtime.pending_events.landed |= events.landed;
    runtime.pending_events.dashed |= events.dashed;
    runtime.pending_events.died |= events.died;
    runtime.pending_events.respawned |= events.respawned;
    runtime.pending_events.hit_wall |= events.hit_wall;
    runtime.pending_events.ability_unlocked |= events.ability_unlocked;
    runtime.pending_events.enemy_defeated |= events.enemy_defeated;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn combat_fixture(
        phase: Phase,
        enemy_position: [f32; 2],
    ) -> (prism_ecs::World, PlatformerBindings, PlayerState) {
        let mut ecs = prism_ecs::World::new();
        let mut world = GameWorld::host(&mut ecs);
        let player = world.spawn();
        world.set(
            player,
            LocalTransform {
                position: glam::Vec3::ZERO,
                ..LocalTransform::default()
            },
        );
        world.set(
            player,
            Collider2D {
                half_extents: Tuning::default().player_half_size,
                layer: SOLID_LAYER,
                ..Collider2D::default()
            },
        );
        world.set(
            player,
            Hurtbox2D {
                team: PLAYER_TEAM,
                enabled: true,
            },
        );
        world.set(player, Active(true));

        let hitbox = world.spawn();
        world.set(hitbox, LocalTransform::default());
        world.set(
            hitbox,
            Collider2D {
                half_extents: DASH_HITBOX_HALF_EXTENTS,
                is_trigger: true,
                layer: DASH_HITBOX_LAYER,
                ..Collider2D::default()
            },
        );
        world.set(hitbox, Active(true));

        let enemy = world.spawn();
        world.set(
            enemy,
            LocalTransform {
                position: glam::Vec3::new(enemy_position[0], enemy_position[1], 0.0),
                ..LocalTransform::default()
            },
        );
        world.set(
            enemy,
            Collider2D {
                half_extents: [6.0, 8.0],
                is_trigger: true,
                layer: ENEMY_LAYER,
                ..Collider2D::default()
            },
        );
        world.set(
            enemy,
            Hurtbox2D {
                team: ENEMY_TEAM,
                enabled: true,
            },
        );
        world.set(enemy, Active(true));
        drop(world);

        let mut player_state = PlayerState::default();
        player_state.phase = phase;
        player_state.position = [0.0, 0.0];
        player_state.velocity = [240.0, 0.0];
        player_state.facing = 1.0;
        player_state.enemies = vec![EnemyState::new(EnemyDefinition {
            id: "test-enemy".to_string(),
            spawn: enemy_position,
            patrol: [
                enemy_position[0] - 6.0,
                enemy_position[1],
                enemy_position[0] + 6.0,
                enemy_position[1],
            ],
            speed: 0.0,
            half_size: [6.0, 8.0],
            health: 1,
        })];
        let bindings = PlatformerBindings {
            player: Some(player),
            player_hitbox: Some(hitbox),
            enemies: vec![enemy],
            ..PlatformerBindings::default()
        };
        let mut world = GameWorld::host(&mut ecs);
        sync_player_hitbox(&mut world, hitbox, &player_state, phase == Phase::Dash);
        drop(world);
        (ecs, bindings, player_state)
    }

    fn frame(right: bool, jump: bool, dash: bool) -> InputFrame {
        InputFrame {
            right,
            jump_pressed: jump,
            jump_held: jump,
            dash_pressed: dash,
            ..InputFrame::default()
        }
    }

    #[test]
    fn dash_damage_uses_engine_hitbox_query_and_disables_defeated_hurtbox() {
        let (mut ecs, bindings, mut player) = combat_fixture(Phase::Dash, [16.0, 0.0]);
        let mut world = GameWorld::host(&mut ecs);
        sync_player_transform(&mut world, bindings.player.unwrap(), player.position);

        let (died, defeated) = resolve_enemy_contacts(&mut world, &bindings, &mut player, true);

        assert!(!died);
        assert!(defeated);
        assert_eq!(player.enemies[0].health, 0);
        assert!(!player.enemies[0].alive);
        assert!(world
            .get::<Hurtbox2D>(bindings.enemies[0])
            .is_some_and(|hurtbox| hurtbox.team == ENEMY_TEAM && !hurtbox.enabled));
        assert!(world
            .damage_contacts2d(DASH_HITBOX_LAYER, ENEMY_LAYER)
            .is_empty());
    }

    #[test]
    fn ordinary_body_contact_with_enemy_remains_lethal() {
        let (mut ecs, bindings, mut player) = combat_fixture(Phase::Normal, [8.0, 0.0]);
        let mut world = GameWorld::host(&mut ecs);
        sync_player_transform(&mut world, bindings.player.unwrap(), player.position);

        let (died, defeated) = resolve_enemy_contacts(&mut world, &bindings, &mut player, false);

        assert!(died);
        assert!(!defeated);
        assert_eq!(player.phase, Phase::Dead);
        assert_eq!(player.enemies[0].health, 1);
    }

    #[test]
    fn space_snapshot_maps_to_a_jump_edge_and_held_input() {
        let mut snapshot = InputSnapshot::default();
        snapshot.set_key(keys::SPACE, true);
        snapshot.set_key_edge(keys::SPACE, true);

        let input = InputFrame::from_snapshot(snapshot);

        assert!(input.jump_pressed);
        assert!(input.jump_held);
    }

    #[test]
    fn reservoir_entry_does_not_overlap_its_return_exit() {
        let manifest: RoomManifest = serde_json::from_str(include_str!("../../assets/rooms.json"))
            .expect("room manifest should be valid JSON");
        let rooms: Vec<_> = manifest
            .rooms
            .into_iter()
            .map(RoomFile::definition)
            .collect();
        let reservoir = rooms
            .iter()
            .position(|room| room.id == "reservoir")
            .expect("reservoir room should exist");
        let entry = rooms[0].exits[0].entry;
        let spawn = rooms[reservoir].geometry.spawn;
        let half = Tuning::default().player_half_size;

        for position in [entry, spawn] {
            let body = Aabb::new(
                position[0] - half[0],
                position[1] - half[1],
                position[0] + half[0],
                position[1] + half[1],
            );
            assert_eq!(
                find_room_transition(&rooms, reservoir, body, AbilityFlags::with_dash()),
                None,
                "reservoir entry and saved spawn must clear the return exit"
            );
        }
    }

    #[test]
    fn coyote_time_allows_a_jump_after_leaving_a_ledge() {
        let mut state = PlayerState::default();
        state.geometry.solids = vec![Aabb::new(0.0, -8.0, 20.0, 8.0)];
        state.geometry.hazards.clear();
        state.position = [17.0, 12.0];
        state.tuning.player_half_size = [4.0, 4.0];
        state.grounded = true;
        for _ in 0..24 {
            state.step(frame(true, false, false), FIXED_DT);
            if !state.grounded {
                break;
            }
        }
        assert!(!state.grounded, "player should have left the platform");
        assert!(state.coyote_timer > 0.0);
        state.step(frame(false, true, false), FIXED_DT);
        assert!(state.velocity[1] > 0.0);
    }

    #[test]
    fn jump_is_available_on_the_first_frame_at_the_room_spawn() {
        let mut state = PlayerState::default();

        assert!(state.grounded);
        state.step(frame(false, true, false), FIXED_DT);

        assert!(state.velocity[1] > 0.0);
        assert!(!state.grounded);
    }

    #[test]
    fn unlocked_double_jump_consumes_one_air_jump_and_cannot_repeat() {
        let mut state = PlayerState::default();
        state.geometry.solids = vec![Aabb::new(-20.0, 0.0, 20.0, 4.0)];
        state.geometry.hazards.clear();
        state.position = [0.0, 8.0];
        state.tuning.player_half_size = [4.0, 4.0];
        state.grounded = true;
        state.tuning.gravity = 0.0;
        state.unlock(AbilityFlags::DOUBLE_JUMP);

        state.step(frame(false, true, false), FIXED_DT);
        assert!(!state.grounded);
        assert_eq!(state.air_jumps_remaining, 1);

        state.step(InputFrame::default(), FIXED_DT);
        state.step(frame(false, true, false), FIXED_DT);
        assert_eq!(state.air_jumps_remaining, 0);
        assert_eq!(state.velocity[1], state.tuning.jump_speed);

        state.velocity[1] = 42.0;
        state.step(InputFrame::default(), FIXED_DT);
        state.step(frame(false, true, false), FIXED_DT);
        assert_eq!(state.air_jumps_remaining, 0);
        assert!(state.velocity[1] < state.tuning.jump_speed * 0.5);
    }

    #[test]
    fn down_plus_jump_drops_through_a_one_way_platform() {
        let mut state = PlayerState::default();
        state.geometry.solids.clear();
        state.geometry.one_way_platforms = vec![Aabb::new(0.0, 0.0, 12.0, 2.0)];
        state.geometry.hazards.clear();
        state.position = [6.0, 3.0];
        state.grounded = true;

        state.step(
            InputFrame {
                down: true,
                jump_pressed: true,
                jump_held: true,
                ..InputFrame::default()
            },
            FIXED_DT,
        );

        assert!(!state.grounded);
        assert!(state.position[1] < 3.0);
    }

    #[test]
    fn wall_jump_uses_the_2d_raycast_near_wall_tolerance() {
        let mut state = PlayerState::default();
        state.geometry.solids = vec![Aabb::new(0.0, 20.0, 10.0, 80.0)];
        state.position = [16.0, 45.0];
        state.grounded = false;
        state.unlock(AbilityFlags::WALL_JUMP);

        state.step(frame(false, true, false), FIXED_DT);

        assert!(
            state.velocity[0] > 0.0,
            "the jump pushes away from the left wall"
        );
        assert!(state.velocity[1] > 0.0, "the jump launches upward");
    }

    #[test]
    fn buffered_jump_triggers_on_landing() {
        let mut state = PlayerState::default();
        state.geometry.solids = vec![Aabb::new(-20.0, -8.0, 20.0, 0.0)];
        state.geometry.hazards.clear();
        state.position = [0.0, 5.0];
        state.tuning.player_half_size = [4.0, 4.0];
        state.velocity[1] = -10.0;
        state.grounded = false;
        state.step(frame(false, true, false), FIXED_DT);
        let mut jumped = false;
        for _ in 0..8 {
            let previous_velocity = state.velocity[1];
            state.step(InputFrame::default(), FIXED_DT);
            if state.velocity[1] > 0.0 && previous_velocity <= 0.0 {
                jumped = true;
                break;
            }
        }
        assert!(
            jumped,
            "the buffered jump should fire on the first grounded tick"
        );
    }

    #[test]
    fn dash_is_eight_directional_and_consumed_until_landing() {
        let mut state = PlayerState::default();
        state.geometry.solids.clear();
        state.geometry.dash_gate = Aabb::new(10_000.0, 10_000.0, 10_008.0, 10_064.0);
        state.unlock(AbilityFlags::DASH);
        let events = state.step(
            InputFrame {
                right: true,
                up: true,
                dash_pressed: true,
                ..InputFrame::default()
            },
            FIXED_DT,
        );
        assert!(events.dashed);
        assert_eq!(state.phase, Phase::Dash);
        assert!((state.velocity[0] - state.velocity[1]).abs() < 0.01);
        assert!(!state.dash_available);
    }

    #[test]
    fn dash_updates_facing_for_a_leftward_attack() {
        let mut state = PlayerState::default();
        state.geometry.solids.clear();
        state.geometry.dash_gate = Aabb::new(10_000.0, 10_000.0, 10_008.0, 10_064.0);
        state.unlock(AbilityFlags::DASH);

        state.step(
            InputFrame {
                left: true,
                dash_pressed: true,
                ..InputFrame::default()
            },
            FIXED_DT,
        );

        assert_eq!(state.facing, -1.0);
        assert!(state.velocity[0] < 0.0);
    }

    #[test]
    fn ground_collision_stops_at_the_visible_tile_top() {
        let mut state = PlayerState::default();
        state.geometry.solids = vec![Aabb::new(0.0, 0.0, 320.0, 16.0)];
        state.geometry.hazards.clear();
        state.position = [24.0, 25.0];
        state.velocity = [0.0, -600.0];

        state.step(InputFrame::default(), FIXED_DT);

        assert!((state.position[1] - 24.0).abs() < 0.01);
        assert!(state.body().min[1] >= 16.0);
        assert!(state.grounded);
    }

    #[test]
    fn left_wall_collision_stops_at_the_visible_tile_edge() {
        let mut state = PlayerState::default();
        state.geometry.solids = vec![
            Aabb::new(0.0, 0.0, 320.0, 16.0),
            Aabb::new(0.0, 16.0, 16.0, 192.0),
        ];
        state.geometry.hazards.clear();
        state.position = [24.0, 24.0];
        state.velocity = [-600.0, 0.0];

        state.step(InputFrame::default(), FIXED_DT);

        assert!((state.position[0] - 22.0).abs() < 0.01);
        assert!(state.body().min[0] >= 16.0);
    }

    #[test]
    fn falling_past_a_floor_does_not_push_left_on_horizontal_sweep() {
        let mut state = PlayerState::default();
        state.geometry.solids = vec![Aabb::new(0.0, 0.0, 320.0, 16.0)];
        state.geometry.hazards.clear();
        state.position = [24.0, 25.0];
        state.velocity = [-60.0, -60.0];

        state.step(InputFrame::default(), FIXED_DT);

        assert!(state.position[0] < 24.0);
        assert!(state.position[0] > 16.0);
    }

    #[test]
    fn room_manifest_loads_stable_ids_exits_and_ability_requirements() {
        let manifest: RoomManifest = serde_json::from_str(
            r#"{"rooms":[
                {"id":"well","bounds":[0,0,320,180],"hazards":[],"dash_gate":[0,0,0,0],"crystal":[0,0],"spawn":[8,8],"columns":20,"tiles":[],"exits":[{"rect":[300,0,320,80],"target":"vault","entry":[12,24],"required_ability":"dash"}]},
                {"id":"vault","bounds":[0,0,320,180],"hazards":[],"dash_gate":[0,0,0,0],"crystal":[0,0],"spawn":[12,24],"columns":20,"tiles":[],"exits":[]}
            ]}"#,
        )
        .unwrap();
        let rooms: Vec<_> = manifest
            .rooms
            .into_iter()
            .map(RoomFile::definition)
            .collect();
        let body = Aabb::new(306.0, 20.0, 318.0, 36.0);

        assert_eq!(
            find_room_transition(&rooms, 0, body, AbilityFlags::empty()),
            None
        );
        assert_eq!(
            find_room_transition(&rooms, 0, body, AbilityFlags::with_dash()),
            Some((1, [12.0, 24.0]))
        );
    }

    #[test]
    fn shipped_room_manifest_has_resolvable_bidirectional_exits() {
        let manifest: RoomManifest = serde_json::from_str(include_str!("../../assets/rooms.json"))
            .expect("room manifest should be valid JSON");
        let rooms: Vec<_> = manifest
            .rooms
            .into_iter()
            .map(RoomFile::definition)
            .collect();
        let ids: std::collections::HashSet<_> = rooms.iter().map(|room| room.id.as_str()).collect();

        assert_eq!(rooms.len(), 2);
        assert!(rooms.iter().all(|room| room_ids_resolve(room, &ids)));
        assert_eq!(rooms[0].geometry.tiles.len(), 20 * 12);
        assert_eq!(rooms[1].geometry.tiles.len(), 20 * 12);
        assert_eq!(rooms[0].geometry.one_way_tile_ids, vec![4]);
        assert!(rooms[1].geometry.one_way_tile_ids.is_empty());
        assert_eq!(
            rooms[0].geometry.solids,
            vec![
                Aabb::new(0.0, 176.0, 16.0, 192.0),
                Aabb::new(304.0, 176.0, 320.0, 192.0),
                Aabb::new(0.0, 160.0, 16.0, 176.0),
                Aabb::new(304.0, 160.0, 320.0, 176.0),
                Aabb::new(0.0, 144.0, 16.0, 160.0),
                Aabb::new(304.0, 144.0, 320.0, 160.0),
                Aabb::new(0.0, 128.0, 16.0, 144.0),
                Aabb::new(304.0, 128.0, 320.0, 144.0),
                Aabb::new(0.0, 112.0, 16.0, 128.0),
                Aabb::new(304.0, 112.0, 320.0, 128.0),
                Aabb::new(0.0, 96.0, 16.0, 112.0),
                Aabb::new(304.0, 96.0, 320.0, 112.0),
                Aabb::new(0.0, 80.0, 16.0, 96.0),
                Aabb::new(304.0, 80.0, 320.0, 96.0),
                Aabb::new(0.0, 64.0, 16.0, 80.0),
                Aabb::new(144.0, 64.0, 208.0, 80.0),
                Aabb::new(0.0, 48.0, 16.0, 64.0),
                Aabb::new(0.0, 32.0, 16.0, 48.0),
                Aabb::new(32.0, 32.0, 112.0, 48.0),
                Aabb::new(0.0, 16.0, 16.0, 32.0),
                Aabb::new(0.0, 0.0, 320.0, 16.0),
            ]
        );
        assert_eq!(
            rooms[0].geometry.one_way_platforms,
            vec![Aabb::new(224.0, 32.0, 272.0, 48.0)]
        );
        assert_eq!(
            rooms[1].geometry.solids,
            vec![
                Aabb::new(0.0, 176.0, 16.0, 192.0),
                Aabb::new(304.0, 176.0, 320.0, 192.0),
                Aabb::new(0.0, 160.0, 16.0, 176.0),
                Aabb::new(304.0, 160.0, 320.0, 176.0),
                Aabb::new(0.0, 144.0, 16.0, 160.0),
                Aabb::new(304.0, 144.0, 320.0, 160.0),
                Aabb::new(0.0, 128.0, 16.0, 144.0),
                Aabb::new(304.0, 128.0, 320.0, 144.0),
                Aabb::new(0.0, 112.0, 16.0, 128.0),
                Aabb::new(304.0, 112.0, 320.0, 128.0),
                Aabb::new(0.0, 96.0, 16.0, 112.0),
                Aabb::new(304.0, 96.0, 320.0, 112.0),
                Aabb::new(0.0, 80.0, 16.0, 96.0),
                Aabb::new(240.0, 80.0, 288.0, 96.0),
                Aabb::new(304.0, 80.0, 320.0, 96.0),
                Aabb::new(304.0, 64.0, 320.0, 80.0),
                Aabb::new(144.0, 48.0, 224.0, 64.0),
                Aabb::new(304.0, 48.0, 320.0, 64.0),
                Aabb::new(48.0, 32.0, 112.0, 48.0),
                Aabb::new(304.0, 32.0, 320.0, 48.0),
                Aabb::new(304.0, 16.0, 320.0, 32.0),
                Aabb::new(0.0, 0.0, 320.0, 16.0),
            ]
        );
        assert_eq!(rooms[0].exits[0].required_ability, Some(AbilityFlags::DASH));
        assert_eq!(rooms[1].exits[0].required_ability, None);
        for row in 7..11 {
            assert_eq!(rooms[0].geometry.tiles[row * 20 + 19], 0);
            assert_eq!(rooms[1].geometry.tiles[row * 20], 0);
        }
        assert_eq!(rooms[0].geometry.tiles[11 * 20 + 19], 1);
        assert_eq!(rooms[1].geometry.tiles[11 * 20], 1);
        assert_eq!(
            find_room_transition(
                &rooms,
                0,
                rooms[0].exits[0].trigger,
                AbilityFlags::with_dash()
            ),
            Some((1, rooms[0].exits[0].entry)),
            "the well exit should resolve to the reservoir entry"
        );
    }

    #[test]
    fn collider_gate_unlock_and_room_exit_work_through_game_world() {
        let manifest: RoomManifest = serde_json::from_str(include_str!("../../assets/rooms.json"))
            .expect("room manifest should be valid JSON");
        let rooms: Vec<_> = manifest
            .rooms
            .into_iter()
            .map(RoomFile::definition)
            .collect();
        let geometry = rooms[0].geometry.clone();
        let mut player = PlayerState::new_in_room("well", Tuning::default(), geometry.clone());
        player.position = [270.0, 60.0];
        player.tuning.gravity = 0.0;

        let mut ecs = prism_ecs::World::new();
        let mut world = GameWorld::host(&mut ecs);
        let player_entity = world.spawn();
        world.set(
            player_entity,
            LocalTransform {
                position: glam::Vec3::new(player.position[0], player.position[1], 0.0),
                ..LocalTransform::default()
            },
        );
        world.set(
            player_entity,
            Collider2D {
                half_extents: player.tuning.player_half_size,
                layer: SOLID_LAYER,
                ..Collider2D::default()
            },
        );
        world.set(player_entity, Active(true));
        assert!(world.enable_transform_interpolation2d(player_entity));

        let gate_entity = spawn_solid_collider(&mut world, 99, geometry.dash_gate);
        let tilemap_entity = world.spawn();
        world.set(tilemap_entity, LocalTransform::default());
        world.set(
            tilemap_entity,
            Tilemap {
                columns: geometry.columns,
                cell_size: [16.0, 16.0],
                tiles: geometry.tiles.clone(),
                ..Tilemap::default()
            },
        );
        world.set(
            tilemap_entity,
            TilemapCollider2D {
                solid_tile_ids: vec![1, 2, 3],
                one_way_tile_ids: geometry.one_way_tile_ids.clone(),
                layer: SOLID_LAYER,
                ..TilemapCollider2D::default()
            },
        );
        world.set(tilemap_entity, Active(true));
        let crystal_entity = world.spawn();
        world.set(
            crystal_entity,
            LocalTransform {
                position: glam::Vec3::new(player.position[0], player.position[1], 0.0),
                ..LocalTransform::default()
            },
        );
        world.set(
            crystal_entity,
            Collider2D {
                half_extents: [8.0, 8.0],
                is_trigger: true,
                layer: PICKUP_LAYER,
                ..Collider2D::default()
            },
        );
        world.set(crystal_entity, Active(true));
        let exit_trigger = world.spawn();
        world.set(
            exit_trigger,
            LocalTransform {
                position: glam::Vec3::new(308.0, 48.0, 0.0),
                ..LocalTransform::default()
            },
        );
        world.set(
            exit_trigger,
            Collider2D {
                half_extents: [12.0, 32.0],
                is_trigger: true,
                layer: EXIT_LAYER,
                ..Collider2D::default()
            },
        );
        world.set(exit_trigger, Active(true));
        let bindings = PlatformerBindings {
            player: Some(player_entity),
            crystal: Some(crystal_entity),
            gate: Some(gate_entity),
            tilemap: Some(tilemap_entity),
            exit_triggers: vec![exit_trigger],
            ..PlatformerBindings::default()
        };
        let blocked = world.move_and_slide2d(
            Some(player_entity),
            player.position,
            player.tuning.player_half_size,
            [40.0, 0.0],
            SOLID_LAYER,
        );
        assert!((blocked.position[0] - 290.0).abs() < 1e-3);
        assert!(blocked.hit_x);
        let runtime = PlatformerRuntime {
            player,
            bindings,
            geometry: geometry.clone(),
            rooms,
            current_room: 0,
            camera_bounds: [0.0, 0.0, 320.0, 180.0],
            tuning_reload_timer: 1.0,
            pending_events: StepEvents::default(),
            input_actions: platformer_input_map(),
            debug_enabled: false,
        };
        let mut state = Some(runtime);

        let input = InputSnapshot::default();
        world.set_input(input);
        fixed_tick(&mut state, &mut world, FIXED_DT);
        assert!(state
            .as_ref()
            .unwrap()
            .player
            .abilities
            .contains(AbilityFlags::DASH));
        frame_tick(&mut state, &mut world, FIXED_DT);
        assert_eq!(world.get::<Active>(gate_entity), Some(Active(false)));
        assert!(world
            .colliders2d(SOLID_LAYER)
            .into_iter()
            .all(|collider| collider.entity != gate_entity));
        state.as_mut().unwrap().player.position = [300.0, 60.0];
        let mut transform = world
            .get::<LocalTransform>(player_entity)
            .expect("player transform exists");
        transform.position = glam::Vec3::new(300.0, 60.0, 0.0);
        world.set(player_entity, transform);
        drop(world);

        prism_engine::physics::update_trigger_events2d(&mut ecs);
        let mut world = GameWorld::host(&mut ecs);
        world.set_input(input);
        fixed_tick(&mut state, &mut world, FIXED_DT);
        let transitioned = state
            .as_ref()
            .is_some_and(|runtime| runtime.player.room_id == "reservoir");
        assert!(
            transitioned,
            "a fixed-step trigger Enter event should transition the room"
        );

        let runtime = state.as_ref().unwrap();
        assert_eq!(runtime.player.room_id, "reservoir");
        assert_eq!(runtime.current_room, 1);
        assert_eq!(runtime.player.position, [32.0, 48.0]);
        assert_eq!(runtime.geometry.solids, runtime.rooms[1].geometry.solids);
        let transform = world
            .get::<LocalTransform>(player_entity)
            .expect("room entry synchronizes the player transform");
        assert_eq!([transform.position.x, transform.position.y], [32.0, 48.0]);
        assert!(world.colliders2d(SOLID_LAYER).iter().any(|collider| {
            collider.entity == tilemap_entity
                && collider.bounds == Aabb::new(48.0, 32.0, 112.0, 48.0)
        }));
        assert_eq!(
            world
                .get::<prism_scene::components::TransformInterpolation2D>(player_entity)
                .unwrap()
                .previous,
            [32.0, 48.0]
        );
    }

    #[test]
    fn shipped_route_reaches_reservoir_from_well_spawn() {
        let manifest: RoomManifest = serde_json::from_str(include_str!("../../assets/rooms.json"))
            .expect("room manifest should be valid JSON");
        let rooms: Vec<_> = manifest
            .rooms
            .into_iter()
            .map(RoomFile::definition)
            .collect();
        let geometry = rooms[0].geometry.clone();
        let player = PlayerState::new_in_room("well", Tuning::default(), geometry.clone());
        assert_eq!(player.position, [24.0, 24.0]);

        let mut ecs = prism_ecs::World::new();
        let mut world = GameWorld::host(&mut ecs);
        let player_entity = world.spawn();
        world.set(
            player_entity,
            LocalTransform {
                position: glam::Vec3::new(player.position[0], player.position[1], 0.0),
                ..LocalTransform::default()
            },
        );
        world.set(
            player_entity,
            Collider2D {
                half_extents: player.tuning.player_half_size,
                layer: SOLID_LAYER,
                ..Collider2D::default()
            },
        );
        world.set(player_entity, Active(true));
        assert!(world.enable_transform_interpolation2d(player_entity));

        let tilemap_entity = world.spawn();
        world.set(tilemap_entity, LocalTransform::default());
        world.set(
            tilemap_entity,
            Tilemap {
                columns: geometry.columns,
                cell_size: [16.0, 16.0],
                tiles: geometry.tiles.clone(),
                ..Tilemap::default()
            },
        );
        world.set(
            tilemap_entity,
            TilemapCollider2D {
                solid_tile_ids: vec![1, 2, 3],
                one_way_tile_ids: geometry.one_way_tile_ids.clone(),
                layer: SOLID_LAYER,
                ..TilemapCollider2D::default()
            },
        );
        world.set(tilemap_entity, Active(true));

        let mut spike_entities = Vec::new();
        for (index, hazard) in geometry.hazards.iter().copied().enumerate() {
            let entity = world.spawn();
            let center = [
                (hazard.min[0] + hazard.max[0]) * 0.5,
                (hazard.min[1] + hazard.max[1]) * 0.5,
            ];
            world.set(
                entity,
                LocalTransform {
                    position: glam::Vec3::new(center[0], center[1], 0.0),
                    ..LocalTransform::default()
                },
            );
            world.set(
                entity,
                Collider2D {
                    half_extents: [
                        (hazard.max[0] - hazard.min[0]) * 0.5,
                        (hazard.max[1] - hazard.min[1]) * 0.5,
                    ],
                    is_trigger: true,
                    layer: HAZARD_LAYER,
                    ..Collider2D::default()
                },
            );
            world.set(entity, Name(format!("spike-{index}")));
            world.set(entity, Active(true));
            spike_entities.push(entity);
        }

        let gate_entity = spawn_solid_collider(&mut world, 99, geometry.dash_gate);
        let crystal_entity = world.spawn();
        world.set(
            crystal_entity,
            LocalTransform {
                position: glam::Vec3::new(geometry.crystal[0], geometry.crystal[1], 0.0),
                ..LocalTransform::default()
            },
        );
        world.set(
            crystal_entity,
            Collider2D {
                half_extents: [8.0, 8.0],
                is_trigger: true,
                layer: PICKUP_LAYER,
                ..Collider2D::default()
            },
        );
        world.set(crystal_entity, Active(true));

        let exit = rooms[0].exits[0].trigger;
        let exit_entity = world.spawn();
        world.set(
            exit_entity,
            LocalTransform {
                position: glam::Vec3::new(
                    (exit.min[0] + exit.max[0]) * 0.5,
                    (exit.min[1] + exit.max[1]) * 0.5,
                    0.0,
                ),
                ..LocalTransform::default()
            },
        );
        world.set(
            exit_entity,
            Collider2D {
                half_extents: [
                    (exit.max[0] - exit.min[0]) * 0.5,
                    (exit.max[1] - exit.min[1]) * 0.5,
                ],
                is_trigger: true,
                layer: EXIT_LAYER,
                ..Collider2D::default()
            },
        );
        world.set(exit_entity, Active(true));

        let bindings = PlatformerBindings {
            player: Some(player_entity),
            crystal: Some(crystal_entity),
            gate: Some(gate_entity),
            tilemap: Some(tilemap_entity),
            spikes: spike_entities,
            exit_triggers: vec![exit_entity],
            ..PlatformerBindings::default()
        };
        let runtime = PlatformerRuntime {
            player,
            bindings,
            geometry: geometry.clone(),
            rooms,
            current_room: 0,
            camera_bounds: [0.0, 0.0, 320.0, 180.0],
            tuning_reload_timer: 1.0,
            pending_events: StepEvents::default(),
            input_actions: platformer_input_map(),
            debug_enabled: false,
        };
        let mut state = Some(runtime);
        let mut jump_hold = 0usize;
        let mut first_jump = true;
        let mut first_spike_jump = false;
        let mut second_spike_jump = false;
        let mut dash_started = false;
        let mut saw_dash = false;

        for _ in 0..360 {
            let (position, grounded, phase, abilities) = {
                let runtime = state.as_ref().expect("runtime should remain alive");
                (
                    runtime.player.position,
                    runtime.player.grounded,
                    runtime.player.phase,
                    runtime.player.abilities,
                )
            };
            assert_ne!(
                phase,
                Phase::Dead,
                "the shipped route killed the player at {position:?}"
            );

            let mut jump = jump_hold > 0;
            if jump_hold > 0 {
                jump_hold -= 1;
            }
            if first_jump {
                first_jump = false;
                jump = true;
                jump_hold = 22;
            } else if grounded && !first_spike_jump && position[0] > 94.0 {
                first_spike_jump = true;
                jump = true;
                jump_hold = 22;
            } else if grounded && !second_spike_jump && position[0] > 250.0 {
                second_spike_jump = true;
                jump = true;
                jump_hold = 22;
            }

            let dash =
                abilities.contains(AbilityFlags::DASH) && !dash_started && position[0] > 274.0;
            dash_started |= dash;
            saw_dash |= dash;

            let mut input = InputSnapshot::default();
            input.set_key(keys::KEY_D, true);
            input.set_key(keys::SPACE, jump);
            input.set_key(keys::SHIFT_LEFT, dash);
            if jump {
                input.set_key_edge(keys::SPACE, true);
            }
            if dash {
                input.set_key_edge(keys::SHIFT_LEFT, true);
            }

            drop(world);
            prism_engine::physics::update_trigger_events2d(&mut ecs);
            world = GameWorld::host(&mut ecs);
            world.set_input(input);
            fixed_tick(&mut state, &mut world, FIXED_DT);
            frame_tick(&mut state, &mut world, FIXED_DT);
            if state
                .as_ref()
                .is_some_and(|runtime| runtime.player.room_id == "reservoir")
            {
                break;
            }
        }

        let runtime = state.as_ref().expect("runtime should remain alive");
        assert!(saw_dash, "the route should activate a dash before the gate");
        assert_eq!(runtime.player.room_id, "reservoir");
        assert_eq!(runtime.current_room, 1);
        assert_eq!(runtime.player.position, [32.0, 48.0]);
    }

    #[test]
    fn entering_a_room_preserves_velocity_and_permanent_abilities() {
        let mut player = PlayerState {
            velocity: [120.0, -35.0],
            ..PlayerState::default()
        };
        player.unlock(AbilityFlags::DASH);
        player.grounded = true;
        player.coyote_timer = 0.08;
        let room = RoomDefinition {
            id: "reservoir".to_string(),
            bounds: [0.0, 0.0, 320.0, 180.0],
            geometry: WorldGeometry {
                spawn: [24.0, 48.0],
                ..WorldGeometry::default()
            },
            exits: Vec::new(),
        };

        enter_room(&mut player, &room, [24.0, 48.0]);

        assert_eq!(player.room_id, "reservoir");
        assert_eq!(player.position, [24.0, 48.0]);
        assert_eq!(player.velocity, [120.0, -35.0]);
        assert!(player.abilities.contains(AbilityFlags::DASH));
        assert!(!player.grounded);
        assert_eq!(player.coyote_timer, 0.0);
    }

    #[test]
    fn dead_player_cannot_trigger_a_room_exit() {
        assert!(can_transition_from_phase(Phase::Normal));
        assert!(can_transition_from_phase(Phase::Dash));
        assert!(!can_transition_from_phase(Phase::Dead));
    }

    #[test]
    fn hazard_death_respawns_after_half_second() {
        let mut state = PlayerState {
            position: [120.0, 12.0],
            ..PlayerState::default()
        };
        let events = state.step(InputFrame::default(), FIXED_DT);
        assert!(events.died);
        assert_eq!(state.phase, Phase::Dead);
        for _ in 0..30 {
            state.step(InputFrame::default(), FIXED_DT);
        }
        assert_eq!(state.phase, Phase::Normal);
        assert_eq!(state.position, state.geometry.spawn);
        assert!(state.grounded);
        state.step(frame(false, true, false), FIXED_DT);
        assert!(state.velocity[1] > 0.0);
    }

    #[test]
    fn replay_restores_its_start_state_before_reusing_inputs() {
        let mut state = PlayerState::default();
        state.geometry.hazards.clear();
        state.unlock(AbilityFlags::DASH);
        state.begin_recording();
        let start_position = state.position;
        for _ in 0..4 {
            state.step(frame(true, false, false), FIXED_DT);
        }
        let expected_position = state.position;
        let expected_velocity = state.velocity;
        let expected_facing = state.facing;
        let expected_grounded = state.grounded;

        state.position = [200.0, 120.0];
        state.velocity = [30.0, -80.0];
        state.abilities = AbilityFlags::empty();
        state.deaths = 7;
        state.tuning.gravity = 1.0;
        state.begin_playback();
        assert_eq!(state.position, start_position);
        assert_eq!(state.velocity, [0.0, 0.0]);
        assert!(state.abilities.contains(AbilityFlags::DASH));
        assert_eq!(state.deaths, 0);
        assert_eq!(state.tuning.gravity, Tuning::default().gravity);

        for _ in 0..4 {
            state.step(InputFrame::default(), FIXED_DT);
        }
        assert_eq!(state.position, expected_position);
        assert_eq!(state.velocity, expected_velocity);
        assert_eq!(state.facing, expected_facing);
        assert_eq!(state.grounded, expected_grounded);
    }
}
