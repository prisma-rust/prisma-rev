//! EchoWell 的确定性平台跳跃切片。
//!
//! 玩法状态放在项目侧，渲染和输入仍由 PrismaRev 提供。`step` 是纯状态更新
//! 入口，测试和输入回放都调用它；`tick` 只负责把状态绑定到场景实体。

use prism_game_api::{keys, EntityHandle, GameWorld, InputEdgeBuffer, InputSnapshot};
use prism_scene::components::{
    Active, Light2D, Light2DKind, LocalTransform, Name, ParticleEmitter2D, SpriteAnimation,
    SpriteRenderer, Tilemap,
};
use prism_scene::ui::TextAlign;
use prism_scene::ui::{Anchors, Canvas, CanvasScaleMode, Margin, Node, Pivot, Style, Text};
use serde::{Deserialize, Serialize};

pub const FIXED_DT: f32 = 1.0 / 60.0;
// Function keys use the project API's raw virtual-key slots.
const F1_KEY_INDEX: usize = keys::OTHER_BASE + 0x70;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Normal,
    Dash,
    Dead,
}

impl Default for Phase {
    fn default() -> Self {
        Self::Normal
    }
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

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Aabb {
    pub min: [f32; 2],
    pub max: [f32; 2],
}

impl Aabb {
    pub const fn new(x0: f32, y0: f32, x1: f32, y1: f32) -> Self {
        Self {
            min: [x0, y0],
            max: [x1, y1],
        }
    }

    pub fn overlaps(self, other: Self) -> bool {
        self.min[0] < other.max[0]
            && self.max[0] > other.min[0]
            && self.min[1] < other.max[1]
            && self.max[1] > other.min[1]
    }
}

/// 一个房间的灰盒关卡数据。后续可直接由 Tilemap / Tiled cooker 生成同样的数组。
#[derive(Debug, Clone)]
pub struct WorldGeometry {
    pub solids: Vec<Aabb>,
    pub hazards: Vec<Aabb>,
    pub dash_gate: Aabb,
    pub crystal: [f32; 2],
    pub spawn: [f32; 2],
    pub columns: u32,
    pub tiles: Vec<u32>,
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
            hazards: vec![
                Aabb::new(110.0, 16.0, 140.0, 26.0),
                Aabb::new(270.0, 16.0, 302.0, 26.0),
            ],
            dash_gate: Aabb::new(296.0, 16.0, 304.0, 80.0),
            crystal: [82.0, 56.0],
            spawn: [24.0, 24.0],
            columns: 20,
            tiles: Vec::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct RoomFile {
    #[serde(default)]
    id: String,
    #[serde(default = "default_room_bounds")]
    bounds: [f32; 4],
    solids: Vec<[f32; 4]>,
    hazards: Vec<[f32; 4]>,
    dash_gate: [f32; 4],
    crystal: [f32; 2],
    spawn: [f32; 2],
    columns: u32,
    tiles: Vec<u32>,
    #[serde(default)]
    exits: Vec<ExitFile>,
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
            solids,
            hazards,
            dash_gate,
            crystal,
            spawn,
            columns,
            tiles,
            exits,
        } = self;
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
                solids: solids.into_iter().map(aabb_from_array).collect(),
                hazards: hazards.into_iter().map(aabb_from_array).collect(),
                dash_gate: aabb_from_array(dash_gate),
                crystal,
                spawn,
                columns,
                tiles,
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
    if rooms.iter().any(|room| room.id == "") {
        rooms[0].id = "well".to_string();
    }
    let room_ids: std::collections::HashSet<String> =
        rooms.iter().map(|room| room.id.clone()).collect();
    for room in &mut rooms {
        room.exits.retain(|exit| room_ids.contains(&exit.target));
    }
    rooms
}

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
    player.position = entry;
    player.grounded = false;
    player.coyote_timer = 0.0;
    player.jump_buffer_timer = 0.0;
    player.variable_jump_timer = 0.0;
}

fn interpolate_position(previous: [f32; 2], current: [f32; 2], alpha: f32) -> [f32; 2] {
    let alpha = alpha.clamp(0.0, 1.0);
    [
        previous[0] + (current[0] - previous[0]) * alpha,
        previous[1] + (current[1] - previous[1]) * alpha,
    ]
}

#[cfg(test)]
fn room_ids_resolve(room: &RoomDefinition, ids: &std::collections::HashSet<&str>) -> bool {
    room.exits
        .iter()
        .all(|exit| ids.contains(exit.target.as_str()))
}

fn clamp_camera_axis(position: f32, min: f32, max: f32, half_view: f32) -> f32 {
    let low = min + half_view;
    let high = max - half_view;
    if low <= high {
        position.clamp(low, high)
    } else {
        (min + max) * 0.5
    }
}

fn is_grounded_at(position: [f32; 2], half_size: [f32; 2], solids: &[Aabb]) -> bool {
    let bottom = position[1] - half_size[1];
    solids.iter().any(|solid| {
        (bottom - solid.max[1]).abs() <= 0.01
            && position[0] + half_size[0] > solid.min[0]
            && position[0] - half_size[0] < solid.max[0]
    })
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
        Self {
            left: input.key(keys::KEY_A) || input.key(keys::ARROW_LEFT),
            right: input.key(keys::KEY_D) || input.key(keys::ARROW_RIGHT),
            up: input.key(keys::KEY_W) || input.key(keys::ARROW_UP),
            down: input.key(keys::KEY_S) || input.key(keys::ARROW_DOWN),
            jump_pressed: input.key_pressed(keys::SPACE) || input.key_pressed(keys::KEY_W),
            jump_held: input.key(keys::SPACE) || input.key(keys::KEY_W),
            dash_pressed: input.key_pressed(keys::SHIFT_LEFT)
                || input.key_pressed(keys::SHIFT_RIGHT)
                || input.key_pressed(keys::KEY_Q),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StepEvents {
    pub landed: bool,
    pub dashed: bool,
    pub died: bool,
    pub respawned: bool,
    pub hit_wall: bool,
    pub ability_unlocked: bool,
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

    pub fn next(&mut self) -> Option<InputFrame> {
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

    pub fn begin_playback(&mut self) {
        self.cursor = 0;
        self.playing = !self.frames.is_empty();
        self.recording = false;
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
    respawn_timer: f32,
    deaths: u32,
    abilities: AbilityFlags,
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
            respawn_timer: state.respawn_timer,
            deaths: state.deaths,
            abilities: state.abilities,
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
        state.respawn_timer = self.respawn_timer;
        state.deaths = self.deaths;
        state.abilities = self.abilities;
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
    pub respawn_timer: f32,
    pub deaths: u32,
    pub abilities: AbilityFlags,
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
            respawn_timer: 0.0,
            deaths: 0,
            abilities: AbilityFlags::empty(),
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
        if let Some(recorded) = self.replay.next() {
            return self.step_inner(recorded, dt);
        }
        self.replay.record(input);
        self.step_inner(input, dt)
    }

    fn step_inner(&mut self, input: InputFrame, dt: f32) -> StepEvents {
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
                return StepEvents {
                    respawned: true,
                    ..StepEvents::default()
                };
            }
            return StepEvents::default();
        }

        let mut events = StepEvents::default();
        if input.jump_pressed {
            self.jump_buffer_timer = self.tuning.jump_buffer_seconds;
        } else {
            self.jump_buffer_timer = (self.jump_buffer_timer - dt).max(0.0);
        }

        if self.grounded {
            self.coyote_timer = self.tuning.coyote_seconds;
            self.dash_available = true;
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
                } else if input.jump_pressed && self.abilities.contains(AbilityFlags::WALL_JUMP) {
                    // 在墙边允许蹬墙跳；真正的碰撞方向由本帧横向解析结果决定。
                    let wall = self.near_wall();
                    if wall != 0.0 {
                        self.velocity = [
                            -wall * self.tuning.wall_jump_horizontal_speed,
                            self.tuning.wall_jump_speed,
                        ];
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
        events.hit_wall = self.move_and_collide(dt);
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

        if self
            .geometry
            .hazards
            .iter()
            .any(|hazard| self.body().overlaps(*hazard))
        {
            self.phase = Phase::Dead;
            self.respawn_timer = self.tuning.respawn_seconds;
            self.velocity = [0.0, 0.0];
            self.deaths = self.deaths.saturating_add(1);
            events.died = true;
        }
        events
    }

    fn body(&self) -> Aabb {
        Aabb::new(
            self.position[0] - self.tuning.player_half_size[0],
            self.position[1] - self.tuning.player_half_size[1],
            self.position[0] + self.tuning.player_half_size[0],
            self.position[1] + self.tuning.player_half_size[1],
        )
    }

    fn near_wall(&self) -> f32 {
        for solid in self.active_solids() {
            if self.body().max[1] <= solid.min[1] || self.body().min[1] >= solid.max[1] {
                continue;
            }
            if (self.body().min[0] - solid.max[0]).abs() <= 1.5 {
                return -1.0;
            }
            if (self.body().max[0] - solid.min[0]).abs() <= 1.5 {
                return 1.0;
            }
        }
        0.0
    }

    fn active_solids(&self) -> impl Iterator<Item = Aabb> + '_ {
        self.geometry.solids.iter().copied().chain(
            (!self.abilities.contains(AbilityFlags::DASH)).then_some(self.geometry.dash_gate),
        )
    }

    fn move_and_collide(&mut self, dt: f32) -> bool {
        let mut hit_wall = false;
        let solids: Vec<Aabb> = self.active_solids().collect();
        let dx = self.velocity[0] * dt;
        let dy = self.velocity[1] * dt;
        let x_steps = if dx.abs() > f32::EPSILON {
            (dx.abs().ceil() as usize).clamp(1, 64)
        } else {
            0
        };
        for _ in 0..x_steps {
            self.position[0] += dx / x_steps as f32;
            for solid in solids.iter().copied() {
                // Horizontal resolution only considers surfaces overlapping
                // the player's vertical span. A stationary horizontal axis
                // must not push the player into a wall as a side effect.
                if self.body().max[1] <= solid.min[1] || self.body().min[1] >= solid.max[1] {
                    continue;
                }
                // A platform/floor should be resolved by the vertical sweep
                // while the player is above or below it, never as a side wall.
                if (self.position[1] >= solid.max[1] && self.velocity[1] <= 0.0)
                    || (self.position[1] <= solid.min[1] && self.velocity[1] >= 0.0)
                {
                    continue;
                }
                if !self.body().overlaps(solid) {
                    continue;
                }
                if dx >= 0.0 {
                    self.position[0] = solid.min[0] - self.tuning.player_half_size[0];
                } else {
                    self.position[0] = solid.max[0] + self.tuning.player_half_size[0];
                }
                self.velocity[0] = 0.0;
                hit_wall = true;
            }
        }

        let y_steps = if dy.abs() > f32::EPSILON {
            (dy.abs().ceil() as usize).clamp(1, 64)
        } else {
            0
        };
        for _ in 0..y_steps {
            self.position[1] += dy / y_steps as f32;
            for solid in solids.iter().copied() {
                // Vertical resolution only considers surfaces overlapping the
                // player's horizontal span, preventing a zero vertical move
                // from embedding the player in a platform or wall.
                if self.body().max[0] <= solid.min[0] || self.body().min[0] >= solid.max[0] {
                    continue;
                }
                if !self.body().overlaps(solid) {
                    continue;
                }
                if dy >= 0.0 {
                    self.position[1] = solid.min[1] - self.tuning.player_half_size[1];
                } else {
                    self.position[1] = solid.max[1] + self.tuning.player_half_size[1];
                    self.grounded = true;
                }
                self.velocity[1] = 0.0;
            }
        }
        hit_wall
    }
}

fn approach(value: f32, target: f32, amount: f32) -> f32 {
    if (target - value).abs() <= amount {
        target
    } else {
        value + (target - value).signum() * amount
    }
}

#[derive(Default)]
pub struct PlatformerBindings {
    pub player: Option<EntityHandle>,
    pub camera: Option<EntityHandle>,
    pub hud: Option<EntityHandle>,
    pub debug: Option<EntityHandle>,
    pub crystal: Option<EntityHandle>,
    pub tilemap: Option<EntityHandle>,
    pub gate: Option<EntityHandle>,
    pub spikes: Vec<EntityHandle>,
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
                "tilemap" => bindings.tilemap = Some(entity),
                "dash-gate" => bindings.gate = Some(entity),
                name if name.starts_with("spike-") => bindings.spikes.push(entity),
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
        bindings.spikes.push(spike);
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
    entity
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
    }

    if let Some(crystal) = bindings.crystal {
        set_sprite_center(world, crystal, geometry.crystal);
        if let Some(mut sprite) = world.get::<SpriteRenderer>(crystal) {
            sprite.pivot = [0.5, 0.5];
            world.set(crystal, sprite);
        }
        resize_sprite_from_source(world, crystal, 16.0, 16.0, [16.0, 20.0]);
    }
}

fn sync_room_visuals(
    world: &mut GameWorld<'_>,
    geometry: &WorldGeometry,
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

    align_collision_sprites(world, geometry, bindings, tuning, player_center);
}

fn set_sprite_center(world: &mut GameWorld<'_>, entity: EntityHandle, center: [f32; 2]) {
    if let Some(mut transform) = world.get::<LocalTransform>(entity) {
        transform.position.x = center[0];
        transform.position.y = center[1];
        world.set(entity, transform);
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
    accumulator: f32,
    input_edges: InputEdgeBuffer,
    previous_position: [f32; 2],
    tuning_reload_timer: f32,
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
        if let Some(bits) = world
            .load_data("echowell.abilities")
            .and_then(|value| value.parse::<u32>().ok())
        {
            player.abilities = AbilityFlags::from_bits(bits);
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
            &mut bindings,
            &player.tuning,
            player.position,
        );
        let previous_position = player.position;
        Self {
            player,
            bindings,
            geometry,
            camera_bounds: room.bounds,
            rooms,
            current_room,
            accumulator: 0.0,
            input_edges: InputEdgeBuffer::default(),
            previous_position,
            tuning_reload_timer: 0.0,
            debug_enabled: false,
        }
    }

    fn transition_room(&mut self, world: &mut GameWorld<'_>) -> bool {
        if !can_transition_from_phase(self.player.phase) {
            return false;
        }
        let Some((next, entry)) = find_room_transition(
            &self.rooms,
            self.current_room,
            self.player.body(),
            self.player.abilities,
        ) else {
            return false;
        };
        let room = self.rooms[next].clone();
        self.current_room = next;
        self.camera_bounds = room.bounds;
        self.geometry = room.geometry.clone();
        enter_room(&mut self.player, &room, entry);
        self.previous_position = self.player.position;
        sync_room_visuals(
            world,
            &self.geometry,
            &mut self.bindings,
            &self.player.tuning,
            entry,
        );
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
        self.camera_bounds = self.rooms[index].bounds;
        self.geometry = self.rooms[index].geometry.clone();
        self.player.geometry = self.geometry.clone();
        self.previous_position = self.player.position;
        sync_room_visuals(
            world,
            &self.geometry,
            &mut self.bindings,
            &self.player.tuning,
            self.player.position,
        );
    }

    fn write_back(
        &mut self,
        world: &mut GameWorld<'_>,
        events: StepEvents,
        render_alpha: f32,
    ) {
        let render_position = interpolate_position(
            self.previous_position,
            self.player.position,
            render_alpha,
        );
        if let Some(player) = self.bindings.player {
            if let Some(mut transform) = world.get::<LocalTransform>(player) {
                transform.position.x = render_position[0];
                // Both physics and rendering use the collision-box centre.
                transform.position.y = render_position[1];
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
        if let Some(camera) = self.bindings.camera {
            if let Some(mut transform) = world.get::<LocalTransform>(camera) {
                let [min_x, min_y, max_x, max_y] = self.camera_bounds;
                transform.position.x =
                    clamp_camera_axis(render_position[0], min_x, max_x, 160.0);
                transform.position.y =
                    clamp_camera_axis(render_position[1], min_y, max_y, 90.0);
                world.set(camera, transform);
            }
        }
        if let Some(hud) = self.bindings.hud {
            if let Some(mut text) = world.get::<Text>(hud) {
                text.content = format!(
                    "ECHO WELL   DASH {}   DEATHS {}",
                    if self.player.abilities.contains(AbilityFlags::DASH) {
                        "READY"
                    } else {
                        "LOCKED"
                    },
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
            world.set(
                crystal,
                Active(!self.player.abilities.contains(AbilityFlags::DASH)),
            );
        }
        if let Some(gate) = self.bindings.gate {
            world.set(
                gate,
                Active(!self.player.abilities.contains(AbilityFlags::DASH)),
            );
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
        if self.player.abilities.contains(AbilityFlags::DASH) {
            return false;
        }
        let crystal = Aabb::new(
            self.geometry.crystal[0] - 8.0,
            self.geometry.crystal[1] - 8.0,
            self.geometry.crystal[0] + 8.0,
            self.geometry.crystal[1] + 8.0,
        );
        if !self.player.body().overlaps(crystal) {
            return false;
        }
        self.player.unlock(AbilityFlags::DASH);
        let _ = world.save_data(
            "echowell.abilities",
            &self.player.abilities.bits().to_string(),
        );
        true
    }
}

pub fn tick(state: &mut Option<PlatformerRuntime>, world: &mut GameWorld<'_>, dt: f32) {
    if state.is_none() {
        *state = Some(PlatformerRuntime::build(world));
    }
    let Some(runtime) = state.as_mut() else {
        return;
    };
    let snapshot = world.input();
    let input = InputFrame::from_snapshot(snapshot);
    if snapshot.key_pressed(F1_KEY_INDEX) {
        runtime.debug_enabled = !runtime.debug_enabled;
    }
    if snapshot.key_pressed(keys::KEY_E) {
        if runtime.player.replay.recording {
            runtime.player.replay.recording = false;
        } else {
            runtime.player.begin_recording();
            runtime.accumulator = 0.0;
        }
    }
    if snapshot.key_pressed(keys::ENTER) {
        runtime.player.begin_playback();
        runtime.accumulator = 0.0;
        runtime.input_edges.clear();
        runtime.previous_position = runtime.player.position;
        runtime.sync_replay_room(world);
    }
    if !runtime.player.replay.playing {
        runtime.input_edges.capture(snapshot);
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
    let fixed_dt = 1.0 / runtime.player.tuning.fixed_hz.clamp(1.0, 240.0);
    runtime.accumulator += dt.clamp(0.0, 0.25);
    let mut events = StepEvents::default();
    let mut first_step = true;
    while runtime.accumulator >= fixed_dt {
        let mut frame = input;
        if first_step {
            let mut step_snapshot = snapshot;
            if runtime.player.replay.playing {
                step_snapshot.key_edges = [0; 8];
            }
            let step_snapshot = runtime.input_edges.take_for_step(step_snapshot);
            let step_input = InputFrame::from_snapshot(step_snapshot);
            frame.jump_pressed = step_input.jump_pressed;
            frame.dash_pressed = step_input.dash_pressed;
        } else {
            // A render snapshot may contain the same edge for every catch-up
            // step. Edge actions belong to the first step only.
            frame.jump_pressed = false;
            frame.dash_pressed = false;
        }
        runtime.previous_position = runtime.player.position;
        let mut step_events = runtime.player.step(frame, fixed_dt);
        if step_events.respawned {
            runtime.previous_position = runtime.player.position;
        }
        if runtime.player.phase != Phase::Dead {
            step_events.ability_unlocked = runtime.collect_crystal(world);
            runtime.transition_room(world);
        }
        events.landed |= step_events.landed;
        events.dashed |= step_events.dashed;
        events.died |= step_events.died;
        events.respawned |= step_events.respawned;
        events.hit_wall |= step_events.hit_wall;
        events.ability_unlocked |= step_events.ability_unlocked;
        first_step = false;
        runtime.accumulator -= fixed_dt;
    }
    runtime.write_back(world, events, runtime.accumulator / fixed_dt);
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn space_snapshot_maps_to_a_jump_edge_and_held_input() {
        let mut snapshot = InputSnapshot::default();
        snapshot.set_key(keys::SPACE, true);
        snapshot.set_key_edge(keys::SPACE, true);

        let input = InputFrame::from_snapshot(snapshot);

        assert!(input.jump_pressed);
        assert!(input.jump_held);
    }

    #[test]
    fn render_interpolation_follows_the_fixed_step_remainder() {
        assert_eq!(interpolate_position([0.0, 0.0], [10.0, 20.0], 0.25), [2.5, 5.0]);
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
                {"id":"well","bounds":[0,0,320,180],"solids":[],"hazards":[],"dash_gate":[0,0,0,0],"crystal":[0,0],"spawn":[8,8],"columns":20,"tiles":[],"exits":[{"rect":[300,0,320,80],"target":"vault","entry":[12,24],"required_ability":"dash"}]},
                {"id":"vault","bounds":[0,0,320,180],"solids":[],"hazards":[],"dash_gate":[0,0,0,0],"crystal":[0,0],"spawn":[12,24],"columns":20,"tiles":[],"exits":[]}
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
    fn entering_a_room_preserves_velocity_and_permanent_abilities() {
        let mut player = PlayerState::default();
        player.velocity = [120.0, -35.0];
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
    fn camera_clamps_to_room_edges_and_centers_inside_small_rooms() {
        assert_eq!(clamp_camera_axis(20.0, 0.0, 320.0, 160.0), 160.0);
        assert_eq!(clamp_camera_axis(490.0, 0.0, 500.0, 160.0), 340.0);
        assert_eq!(clamp_camera_axis(12.0, 0.0, 200.0, 160.0), 100.0);
    }

    #[test]
    fn dead_player_cannot_trigger_a_room_exit() {
        assert!(can_transition_from_phase(Phase::Normal));
        assert!(can_transition_from_phase(Phase::Dash));
        assert!(!can_transition_from_phase(Phase::Dead));
    }

    #[test]
    fn hazard_death_respawns_after_half_second() {
        let mut state = PlayerState::default();
        state.position = [120.0, 12.0];
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
