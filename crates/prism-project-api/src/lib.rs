//! Same-build project module contract.
//!
//! This is intentionally **not** a stable third-party ABI yet. The host and
//! project module must be built from the same engine revision, Rust toolchain,
//! profile, and feature set. The function tables are `#[repr(C)]` so the
//! boundary is explicit while the API is still being narrowed.
//!
//! ## Boundary rules
//!
//! Only POD values ([`EntityHandle`] / [`InputSnapshot`]) and byte streams cross
//! the boundary. Component data is RTTIBin-encoded and decoded by the host with
//! its own `ComponentRegistry`, so adding a component type needs no ABI change
//! and no per-component binding here.

/// ABI key indices (bit order of `InputSnapshot::keys`).
///
/// This is the contract between host and project: **append only, never
/// reorder**. The engine's `KeyCode::bit_index` must match this list entry by
/// entry (`prism-engine` has a test for it).
pub mod keys {
    pub const KEY_W: usize = 0;
    pub const KEY_A: usize = 1;
    pub const KEY_S: usize = 2;
    pub const KEY_D: usize = 3;
    pub const KEY_Q: usize = 4;
    pub const KEY_E: usize = 5;
    pub const SPACE: usize = 6;
    pub const SHIFT_LEFT: usize = 7;
    pub const SHIFT_RIGHT: usize = 8;
    pub const CONTROL_LEFT: usize = 9;
    pub const CONTROL_RIGHT: usize = 10;
    pub const ALT_LEFT: usize = 11;
    pub const ALT_RIGHT: usize = 12;
    pub const ESCAPE: usize = 13;
    pub const TAB: usize = 14;
    pub const ENTER: usize = 15;
    pub const ARROW_UP: usize = 16;
    pub const ARROW_DOWN: usize = 17;
    pub const ARROW_LEFT: usize = 18;
    pub const ARROW_RIGHT: usize = 19;
    pub const DIGIT_0: usize = 20;
    /// Platform raw key codes start here (`30 + code % 226`).
    pub const OTHER_BASE: usize = 30;
}

pub const ABI_VERSION: u32 = 13;

pub const SUCCESS: i32 = 0;
pub const INVALID_ARGUMENT: i32 = 1;
pub const REGISTRATION_FAILED: i32 = 2;
pub const TICK_FAILED: i32 = 3;
/// The host does not know this type name (not registered in `ComponentRegistry`).
pub const UNKNOWN_TYPE: i32 = 4;
/// The output buffer is too small; `out_count` has been set to the required length.
pub const BUFFER_TOO_SMALL: i32 = 5;
/// The host does not offer this capability (the matching `HostApi` callback is `None`).
pub const UNSUPPORTED: i32 = 6;

/// Catches Rust panics at the ABI boundary.
///
/// The module exports `extern "C"` functions, and unwinding a panic across an
/// `extern "C"` boundary is **undefined behavior** (recent std aborts the whole
/// process). When the host is the editor, a single `unwrap()` in project code
/// would take the editor down with it, so every exported entry point goes
/// through here: the panic is caught in place, [`TICK_FAILED`] is returned, and
/// the host keeps running and shows the error.
///
/// This deliberately does **not** take over `panic::set_hook`: hooks are
/// process-wide and module and host share one process, so a module hook would
/// displace the host's logging and crash collection. The call stack is the
/// host hook's job (`std::backtrace::Backtrace::force_capture()`; keeping the
/// PDB next to the module resolves module frames).
pub fn guard<F: FnOnce() -> i32>(f: F) -> i32 {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(status) => status,
        Err(_) => TICK_FAILED,
    }
}

pub const HOST_CAP_LOGGING: u64 = 1 << 0;
pub const HOST_CAP_WORLD_COMMANDS: u64 = 1 << 1;
/// Loose asset access (`asset_exists` / `read_asset_file`).
pub const HOST_CAP_ASSET: u64 = 1 << 4;
pub const HOST_CAP_COMPONENTS: u64 = 1 << 2;
pub const HOST_CAP_DRAW: u64 = 1 << 3;
/// Engine-owned scene state (`save_scene_state` / `load_scene_state`, #484).
pub const HOST_CAP_STATE: u64 = 1 << 5;
/// Scene switching (`load_scene` / `unload_scene`, #482). Requests are
/// **deferred**: the host performs the switch after this tick, because the
/// world is borrowed while the callback runs.
pub const HOST_CAP_SCENE: u64 = 1 << 6;
/// Physics queries (`raycast`, #522). The host advertises this bit only after it
/// injects a query implementation.
pub const HOST_CAP_PHYSICS: u64 = 1 << 7;
/// 玩家存档（`save_value` / `load_value`，#984）。与引擎自带的场景存档
/// （[`HOST_CAP_STATE`]）分开：这是工程自己决定 schema 的键值存储。
pub const HOST_CAP_SAVE: u64 = 1 << 8;

/// 一次性音效（`play_sound` / `stop_sound`，#987）。宿主注入音频实现后才会
/// 声明该能力位；没有它时编辑器 Play 是一次性音效的哑巴。
pub const HOST_CAP_AUDIO: u64 = 1 << 9;
/// 2D AABB collider queries (`Collider2D` world bounds).
pub const HOST_CAP_PHYSICS_2D: u64 = 1 << 10;

/// An entity handle crossing the ABI.
///
/// It is layout-isomorphic to the ECS-internal entity handle but **does not
/// share type identity**: inside a plugin the handle is only a host slot index
/// plus a generation, and the host revalidates that it is still alive before
/// every use.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct EntityHandle {
    pub id: u32,
    pub generation: u32,
}

impl EntityHandle {
    /// The null handle; the host must reject it.
    pub const PLACEHOLDER: Self = Self {
        id: u32::MAX,
        generation: u32::MAX,
    };

    pub const fn is_placeholder(self) -> bool {
        self.id == u32::MAX
    }
}

/// Touch phases (values of `phase` in `InputSnapshot::touches`).
pub const TOUCH_PHASE_STARTED: u32 = 0;
pub const TOUCH_PHASE_MOVED: u32 = 1;
pub const TOUCH_PHASE_ENDED: u32 = 2;
pub const TOUCH_PHASE_CANCELLED: u32 = 3;

/// A single touch point (Android / iOS `Touch`).
///
/// Like [`InputSnapshot`] it is POD: the whole `InputSnapshot` crosses the ABI
/// as a block and cannot hold a `Vec`, so the touch count is capped at
/// [`InputSnapshot::MAX_TOUCHES`]. Touches beyond the cap are dropped (the
/// host's `InputManager` still keeps the full list).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TouchSnapshot {
    /// Platform touch id (stable for the whole lifetime of one finger).
    pub id: u64,
    /// Draw-area pixel coordinates (same coordinate system as [`InputSnapshot::cursor`]).
    pub position: [f32; 2],
    /// See `TOUCH_PHASE_*`; unknown values are treated as "moved".
    pub phase: u32,
    /// Reserved; must be 0.
    pub _reserved: u32,
}

impl TouchSnapshot {
    pub const EMPTY: Self = Self {
        id: 0,
        position: [0.0; 2],
        phase: TOUCH_PHASE_CANCELLED,
        _reserved: 0,
    };
}

/// Per-frame input snapshot handed to gameplay code.
///
/// Fixed-size and `#[repr(C)]`, so it crosses the ABI as a block. Keys use a
/// bitmap rather than a `Vec` to avoid heap allocation and ownership questions
/// at the boundary.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct InputSnapshot {
    /// Cursor position (**draw-area pixels**, top-left origin).
    ///
    /// Same coordinate system as [`Self::screen`]: the host already folds
    /// logical coordinates into the render target so `0 <= cursor <= screen`.
    /// Plugins must not apply a DPI scale on top.
    pub cursor: [f32; 2],
    /// Current draw-area size in pixels (same coordinate system as [`Self::cursor`]).
    pub screen: [f32; 2],
    pub left_clicked: bool,
    pub left_held: bool,
    /// Key bitmap: `keys[i / 64] & (1 << (i % 64))`.
    /// Key indices are defined by the host; plugins only look them up.
    pub keys: [u64; 4],

    // These fields were appended in #485: the earlier `left_clicked` /
    // `left_held` / `keys` covered only "left button held", with no outlet for
    // the right button, wheel, mouse delta, touch, or key edges. They sit at the
    // end so ABI differences show up only in `ABI_VERSION`.
    /// Right button pressed this frame.
    pub right_clicked: bool,
    /// Right button held.
    pub right_held: bool,
    /// Middle button pressed this frame.
    pub middle_clicked: bool,
    /// Middle button held.
    pub middle_held: bool,
    /// Mouse delta this frame (pixels, unscaled). Use this for FPS look, not a
    /// `cursor` difference: `cursor` stops moving while the pointer is locked.
    pub mouse_delta: [f32; 2],
    /// Wheel amount this frame in "wheel lines" (one notch of a classic wheel = 1.0).
    pub scroll: f32,
    /// Whether the pointer is locked (`CursorGrabMode::Locked`). While locked,
    /// the mouse is no longer bounded by the screen edge.
    pub pointer_locked: bool,
    /// Key-edge bitmap: the first [`InputSnapshot::KEY_WORDS`] words are
    /// "pressed this frame", the next `KEY_WORDS` are "released this frame".
    ///
    /// `keys` has **held** semantics, so testing "Space pressed this frame" must
    /// use this: differencing `keys` yourself misses a quick tap that pressed
    /// and released within one frame.
    pub key_edges: [u64; 8],
    /// Number of valid touches in `touches` (`0..=MAX_TOUCHES`).
    pub touch_count: u32,
    /// Touch table; only the first `touch_count` entries are valid.
    pub touches: [TouchSnapshot; 10],
    /// Remainder of the host fixed-step accumulator, in `0..=1`.
    pub interpolation_alpha: f32,
}

impl InputSnapshot {
    /// Number of supported key indices.
    pub const KEY_COUNT: usize = 256;
    pub const KEY_WORDS: usize = Self::KEY_COUNT / 64;
    /// All-zero snapshot (for `const` contexts: `HostApi::logging_only`).
    pub const EMPTY: Self = Self {
        cursor: [0.0; 2],
        screen: [0.0; 2],
        left_clicked: false,
        left_held: false,
        keys: [0; Self::KEY_WORDS],
        right_clicked: false,
        right_held: false,
        middle_clicked: false,
        middle_held: false,
        mouse_delta: [0.0; 2],
        scroll: 0.0,
        pointer_locked: false,
        key_edges: [0; 8],
        touch_count: 0,
        touches: [TouchSnapshot::EMPTY; Self::MAX_TOUCHES],
        interpolation_alpha: 0.0,
    };

    pub fn set_key(&mut self, index: usize, down: bool) {
        if index >= Self::KEY_COUNT {
            return;
        }
        let mask = 1u64 << (index % 64);
        let word = &mut self.keys[index / 64];
        if down {
            *word |= mask;
        } else {
            *word &= !mask;
        }
    }

    pub fn key(&self, index: usize) -> bool {
        index < Self::KEY_COUNT && (self.keys[index / 64] & (1u64 << (index % 64))) != 0
    }

    /// Whether any key is held this frame (used by the intro's "press any key to skip").
    pub fn any_key(&self) -> bool {
        self.keys.iter().any(|word| *word != 0)
    }

    /// Touch cap (fixed array length, see [`Self::touches`]).
    pub const MAX_TOUCHES: usize = 10;

    /// Whether this key was **pressed** this frame (edge; independent of the held bitmap).
    pub fn key_pressed(&self, index: usize) -> bool {
        index < Self::KEY_COUNT && (self.key_edges[index / 64] & (1u64 << (index % 64))) != 0
    }

    /// Whether this key was **released** this frame.
    pub fn key_released(&self, index: usize) -> bool {
        index < Self::KEY_COUNT
            && (self.key_edges[Self::KEY_WORDS + index / 64] & (1u64 << (index % 64))) != 0
    }

    /// Sets a key-edge bit (used by the host to fill the table).
    pub fn set_key_edge(&mut self, index: usize, pressed: bool) {
        if index >= Self::KEY_COUNT {
            return;
        }
        let mask = 1u64 << (index % 64);
        let word = if pressed {
            &mut self.key_edges[index / 64]
        } else {
            &mut self.key_edges[Self::KEY_WORDS + index / 64]
        };
        *word |= mask;
    }

    /// The valid touches.
    pub fn touch_slice(&self) -> &[TouchSnapshot] {
        let count = (self.touch_count as usize).min(Self::MAX_TOUCHES);
        &self.touches[..count]
    }

    /// Whether any touch is down (the "screen pressed" test on Android).
    pub fn any_touch(&self) -> bool {
        self.touch_slice()
            .iter()
            .any(|touch| touch.phase != TOUCH_PHASE_ENDED && touch.phase != TOUCH_PHASE_CANCELLED)
    }
}

impl Default for InputSnapshot {
    fn default() -> Self {
        Self::EMPTY
    }
}

/// Whether a loose asset exists: returns 1/0, negative on error.
pub type AssetExists = unsafe extern "C" fn(
    user_data: *mut core::ffi::c_void,
    path: *const u8,
    path_len: usize,
) -> i32;

/// Reads a loose asset; the returned pointer is valid only until the next host call.
pub type ReadAssetFile = unsafe extern "C" fn(
    user_data: *mut core::ffi::c_void,
    path: *const u8,
    path_len: usize,
    out_ptr: *mut *const u8,
    out_len: *mut usize,
) -> i32;

/// Saves the engine scene state; returns [`SUCCESS`] or an error code.
pub type SaveSceneState = unsafe extern "C" fn(user_data: *mut core::ffi::c_void) -> i32;

/// Loads the scene state: returns 1 = restored, 0 = no state, negative on error.
pub type LoadSceneState = unsafe extern "C" fn(user_data: *mut core::ffi::c_void) -> i32;

/// Requests a scene switch (#482): `path` is a UTF-8 `.scene.json` path, and
/// `additive = 0` means `Single` (unload the old scene).
///
/// [`SUCCESS`] only means the request was recorded; the actual switch happens
/// after this tick (`HostApi::scene_generation` increments on the next frame).
/// Failures return a negative value.
pub type LoadScene = unsafe extern "C" fn(
    user_data: *mut core::ffi::c_void,
    path: *const u8,
    path_len: usize,
    additive: u8,
) -> i32;

/// Requests unloading the current scene (`Persistent` subtrees are kept); also deferred.
pub type UnloadScene = unsafe extern "C" fn(user_data: *mut core::ffi::c_void) -> i32;

/// POD result of a single ray hit (#522).
///
/// Isomorphic to the engine's `physics::RayHit`, but entities use the
/// cross-boundary handle. The host normalizes the direction; `distance` is
/// measured from the origin, and `point` / `normal` are in world space.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RaycastHit {
    pub entity: EntityHandle,
    pub distance: f32,
    pub point: [f32; 3],
    pub normal: [f32; 3],
    /// Whether the hit collider is a trigger (`1` / `0`).
    pub is_trigger: u8,
    /// Reserved; must be 0.
    pub _reserved: [u8; 3],
}

/// Physics ray query (#522): returns `1` = hit and `out_hit` written, `0` = miss,
/// negative = error code (null pointer / host has no query capability).
///
/// Arrays are passed by pointer (`*const [f32; 3]` points to three `f32`s), so
/// the caller must keep the pointer valid for the duration of the call; the
/// same applies to `out_hit`.
pub type Raycast = unsafe extern "C" fn(
    user_data: *mut core::ffi::c_void,
    origin: *const [f32; 3],
    direction: *const [f32; 3],
    max_distance: f32,
    layer_mask: u32,
    out_hit: *mut RaycastHit,
) -> i32;

/// One active 2D collider in world space.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Collider2DHit {
    pub entity: EntityHandle,
    /// World-space AABB `[min_x, min_y, max_x, max_y]`.
    pub bounds: [f32; 4],
    pub is_trigger: u8,
    pub _reserved: [u8; 3],
    pub layer: u32,
}

/// Enumerates active 2D colliders matching a layer mask. A null output pointer
/// performs a count-only query; the result is valid until the next host call.
pub type QueryColliders2D = unsafe extern "C" fn(
    user_data: *mut core::ffi::c_void,
    layer_mask: u32,
    out_colliders: *mut Collider2DHit,
    capacity: usize,
    out_count: *mut usize,
) -> i32;

/// 写入一条玩家存档（#984）：`0` = 已落盘，负数为错误码（没有能力 / 写失败）。
///
/// `key`/`value` 都是字节串，schema 由工程自己解释；`value_len == 0` 时
/// `value` 可以是 `null`。
pub type SaveValue = unsafe extern "C" fn(
    user_data: *mut core::ffi::c_void,
    key: *const u8,
    key_len: usize,
    value: *const u8,
    value_len: usize,
) -> i32;

/// 读取一条玩家存档（#984）：返回**完整字节长度**。
///
/// `out == null` 或 `out_cap` 不足时只返回长度（两段读取）；键不存在返回负数。
pub type LoadValue = unsafe extern "C" fn(
    user_data: *mut core::ffi::c_void,
    key: *const u8,
    key_len: usize,
    out: *mut u8,
    out_cap: usize,
) -> i32;

/// 播放一个一次性音效（#987）：`path` 是相对 `assets/` 的 UTF-8 路径，
/// `volume` 是 0..1 的线性增益，`repeat = 1` 表示循环。
///
/// 成功时返回 [`SUCCESS`] 并写 `out_id`（非 0，可用于 `stop_sound`）；
/// 片段不存在 / 解码失败 / 宿主没有音频时返回负数，`out_id` 保持 0。
pub type PlaySound = unsafe extern "C" fn(
    user_data: *mut core::ffi::c_void,
    path: *const u8,
    path_len: usize,
    volume: f32,
    repeat: u8,
    out_id: *mut u32,
) -> i32;

/// 停止 `play_sound` 返回的音效 id；未知或已结束的 id 返回 [`SUCCESS`]。
pub type StopSound = unsafe extern "C" fn(user_data: *mut core::ffi::c_void, id: u32) -> i32;

pub type LogMessage = unsafe extern "C" fn(
    user_data: *mut core::ffi::c_void,
    level: u32,
    message: *const u8,
    message_len: usize,
);

pub type SpawnEntity =
    unsafe extern "C" fn(user_data: *mut core::ffi::c_void, out_entity: *mut EntityHandle) -> i32;

pub type SetEntityName = unsafe extern "C" fn(
    user_data: *mut core::ffi::c_void,
    entity: EntityHandle,
    name: *const u8,
    name_len: usize,
) -> i32;

/// Lists the entities that have a given component.
///
/// With an empty `out` only `out_count` is written (probe the length), otherwise
/// it fills up to `capacity` entries.
pub type QueryEntities = unsafe extern "C" fn(
    user_data: *mut core::ffi::c_void,
    type_name: *const u8,
    type_name_len: usize,
    out_entities: *mut EntityHandle,
    capacity: usize,
    out_count: *mut usize,
) -> i32;

/// Reads a component's RTTIBin encoding.
///
/// The returned pointer is owned by the host and is **valid only until the next
/// host call**; the caller must copy the data before then.
pub type GetComponent = unsafe extern "C" fn(
    user_data: *mut core::ffi::c_void,
    entity: EntityHandle,
    type_name: *const u8,
    type_name_len: usize,
    out_bytes: *mut *const u8,
    out_len: *mut usize,
) -> i32;

/// Writes (or adds) a component. The type is carried by the RTTIBin payload.
pub type SetComponent = unsafe extern "C" fn(
    user_data: *mut core::ffi::c_void,
    entity: EntityHandle,
    bytes: *const u8,
    len: usize,
) -> i32;

pub type RemoveComponent = unsafe extern "C" fn(
    user_data: *mut core::ffi::c_void,
    entity: EntityHandle,
    type_name: *const u8,
    type_name_len: usize,
) -> i32;

/// Destroys an entity (its components are recycled too). The slot may be
/// reused, but the generation increments, so old handles stay invalid.
pub type DespawnEntity =
    unsafe extern "C" fn(user_data: *mut core::ffi::c_void, entity: EntityHandle) -> i32;

/// Whether the handle still points to a live entity: `1` alive, `0` dead,
/// negative = bad arguments.
///
/// Unlike the other commands it returns liveness directly instead of an extra
/// out-parameter.
pub type IsAlive =
    unsafe extern "C" fn(user_data: *mut core::ffi::c_void, entity: EntityHandle) -> i32;

/// A 2D UI quad.
///
/// The fields match the engine-side UI quad so the host can merge it into its own
/// draw list without conversion and the project module does not have to link the
/// render crate.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct UiQuad {
    /// `[x, y, width, height]` in screen pixels, top-left origin.
    pub rect: [f32; 4],
    /// RGBA, linear space.
    pub color: [f32; 4],
    /// Corner radius (px).
    pub border_radius: f32,
    /// Draw order (z-index).
    pub layer: i32,
}

/// Submits a UI quad; the host merges it into its draw list before this frame's UI render.
pub type DrawQuad = unsafe extern "C" fn(user_data: *mut core::ffi::c_void, quad: UiQuad) -> i32;

/// Host capability table.
///
/// Built once per tick by the host; the function pointers and `user_data` are
/// **valid only for this tick**.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct HostApi {
    pub abi_version: u32,
    /// Schema fingerprint of reflected engine components (prevents stale module loads).
    pub schema_fingerprint: u64,
    pub capabilities: u64,
    pub scene_generation: u64,
    pub user_data: *mut core::ffi::c_void,
    pub input: InputSnapshot,
    pub log: Option<LogMessage>,
    pub spawn_entity: Option<SpawnEntity>,
    pub set_entity_name: Option<SetEntityName>,
    pub query_entities: Option<QueryEntities>,
    pub get_component: Option<GetComponent>,
    pub set_component: Option<SetComponent>,
    pub remove_component: Option<RemoveComponent>,
    pub despawn_entity: Option<DespawnEntity>,
    pub is_alive: Option<IsAlive>,
    pub draw_quad: Option<DrawQuad>,
    pub asset_exists: Option<AssetExists>,
    pub read_asset_file: Option<ReadAssetFile>,
    /// Engine-owned save state (#484). When absent, the project module has no persistence.
    pub save_scene_state: Option<SaveSceneState>,
    pub load_scene_state: Option<LoadSceneState>,
    /// Scene switching (#482); capability bit [`HOST_CAP_SCENE`].
    pub load_scene: Option<LoadScene>,
    pub unload_scene: Option<UnloadScene>,
    /// Physics ray query (#522); capability bit [`HOST_CAP_PHYSICS`].
    pub raycast: Option<Raycast>,
    /// 2D collider query; capability bit [`HOST_CAP_PHYSICS_2D`].
    pub query_colliders2d: Option<QueryColliders2D>,
    /// 玩家存档（#984）；能力位 [`HOST_CAP_SAVE`]。
    pub save_value: Option<SaveValue>,
    pub load_value: Option<LoadValue>,

    /// 一次性音效（#987）；能力位 [`HOST_CAP_AUDIO`]。
    pub play_sound: Option<PlaySound>,
    pub stop_sound: Option<StopSound>,
}

impl HostApi {
    /// Minimal table with only the logging capability (used during `create`).
    pub const fn logging_only() -> Self {
        Self {
            abi_version: ABI_VERSION,
            schema_fingerprint: 0,
            capabilities: HOST_CAP_LOGGING,
            scene_generation: 0,
            user_data: core::ptr::null_mut(),
            input: InputSnapshot::EMPTY,
            log: None,
            spawn_entity: None,
            set_entity_name: None,
            query_entities: None,
            get_component: None,
            set_component: None,
            remove_component: None,
            despawn_entity: None,
            is_alive: None,
            draw_quad: None,
            asset_exists: None,
            read_asset_file: None,
            save_scene_state: None,
            load_scene_state: None,
            load_scene: None,
            unload_scene: None,
            raycast: None,
            query_colliders2d: None,
            save_value: None,
            load_value: None,

            play_sound: None,
            stop_sound: None,
        }
    }
}

pub type CreateProject =
    unsafe extern "C" fn(host: *const HostApi, out_state: *mut *mut core::ffi::c_void) -> i32;

/// The host table and its `user_data` are valid only until this call returns.
pub type TickProject =
    unsafe extern "C" fn(host: *const HostApi, state: *mut core::ffi::c_void, dt: f32) -> i32;

/// One deterministic gameplay simulation step.
pub type FixedUpdateProject =
    unsafe extern "C" fn(host: *const HostApi, state: *mut core::ffi::c_void, dt: f32) -> i32;

pub type ShutdownProject = unsafe extern "C" fn(state: *mut core::ffi::c_void);

/// Exported entry point resolved by the runtime host.
pub type GetProjectApi = unsafe extern "C" fn(*mut ProjectApi) -> i32;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ProjectApi {
    pub abi_version: u32,
    /// Schema fingerprint of reflected engine components built into the project module.
    pub schema_fingerprint: u64,
    pub create: Option<CreateProject>,
    pub tick: Option<TickProject>,
    pub fixed_update: Option<FixedUpdateProject>,
    pub shutdown: Option<ShutdownProject>,
}

impl ProjectApi {
    pub const fn empty() -> Self {
        Self {
            abi_version: 0,
            schema_fingerprint: 0,
            create: None,
            tick: None,
            fixed_update: None,
            shutdown: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_bits_round_trip() {
        let mut input = InputSnapshot::default();
        assert!(!input.any_key());

        input.set_key(7, true);
        input.set_key(200, true);
        assert!(input.key(7));
        assert!(input.key(200));
        assert!(!input.key(8));
        assert!(!input.key(300));

        input.set_key(7, false);
        assert!(!input.key(7));
        assert!(input.key(200));
    }

    #[test]
    fn out_of_range_keys_are_ignored() {
        let mut input = InputSnapshot::default();
        input.set_key(usize::MAX, true);
        assert!(!input.any_key());
    }

    #[test]
    fn logging_only_table_advertises_no_world_access() {
        let api = HostApi::logging_only();
        assert_eq!(api.capabilities, HOST_CAP_LOGGING);
        assert!(api.query_entities.is_none());
        assert!(api.set_component.is_none());
        assert!(api.raycast.is_none());
    }
}

#[cfg(test)]
mod input_snapshot_tests {
    use super::*;

    #[test]
    fn key_edges_are_separate_from_the_held_bitmap() {
        let mut snapshot = InputSnapshot::default();
        snapshot.set_key(6, true);
        snapshot.set_key_edge(6, true);
        assert!(snapshot.key(6));
        assert!(snapshot.key_pressed(6));
        assert!(!snapshot.key_released(6));
        assert!(!snapshot.key_pressed(7));

        // Out-of-range indices must not write past the end.
        snapshot.set_key(InputSnapshot::KEY_COUNT, true);
        snapshot.set_key_edge(InputSnapshot::KEY_COUNT, true);
        assert!(!snapshot.key_pressed(InputSnapshot::KEY_COUNT));
    }

    #[test]
    fn touch_slice_only_returns_the_counted_entries() {
        let mut snapshot = InputSnapshot::default();
        assert!(snapshot.touch_slice().is_empty());
        assert!(!snapshot.any_touch());

        snapshot.touches[0] = TouchSnapshot {
            id: 3,
            position: [5.0, 6.0],
            phase: TOUCH_PHASE_MOVED,
            _reserved: 0,
        };
        snapshot.touch_count = 1;
        assert_eq!(snapshot.touch_slice().len(), 1);
        assert!(snapshot.any_touch());

        // Truncates beyond the fixed array length instead of panicking.
        snapshot.touch_count = u32::MAX;
        assert_eq!(snapshot.touch_slice().len(), InputSnapshot::MAX_TOUCHES);
    }

    #[test]
    fn the_snapshot_stays_pod_sized() {
        // `InputSnapshot` must be able to cross the ABI as a block: its fields
        // can only be POD, and it must not silently grow until the `#[repr(C)]`
        // layout depends on the compiler.
        let size = core::mem::size_of::<InputSnapshot>();
        assert!(size <= 512, "InputSnapshot grew to {size} bytes");
        assert!(size >= 4 * 8 + 10 * core::mem::size_of::<TouchSnapshot>());
    }
}
