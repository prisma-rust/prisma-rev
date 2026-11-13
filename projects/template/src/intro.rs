//! 开场 intro screen：黑场淡出 → 标题上浮 → 停留 → 淡出，任意键/点击跳过。
//!
//! 实体由 `assets/scenes/intro.scene.json` 加载（4 个 UI 实体：黑场 + 标题 +
//! 副标题 + 提示）。动画逻辑走 [`prism_game_api::GameWorld`]，因此**同一份代码**
//! 既能跑在静态 Player 里（`tick_system`），也能跑在编辑器加载的动态模块里
//! （`preview` 模块直接调用 [`tick`]）。
//!
//! 本文件不出现任何 `prism_engine` 类型：组件来自纯数据的 `prism_scene`，
//! 引擎/编辑器也不需要认识 `IntroState`。

use keyframe::functions::{EaseInCubic, EaseInOutCubic, EaseOutCubic};
use keyframe::{keyframes, AnimationSequence};
use prism_game_api::{EntityHandle, GameWorld, InputSnapshot, UiQuad};
use prism_scene::ui::{Style, Text};

/// 标题初始位置（margin.top，CENTER 锚点下负值 = 中心上方）。
const TITLE_BASE_MARGIN: f32 = -16.0;
/// 副标题相对标题的纵向间距（px）。
///
/// 标题矩形高 108、`pivot.y = 0.5`，副标题高 42，两者中心至少要拉开
/// `(108 + 42) / 2 = 75` 才不重叠；但标题字形（84px）的墨迹会超出自己的框，
/// 实测 96 时副标题仍压在标题下沿，128 才留出干净的间距。
/// `intro.scene.json` 里 `intro_subtitle` 的 `margin.top` 必须等于
/// `TITLE_BASE_MARGIN + SUBTITLE_OFFSET`（每帧由 `tick_system` 覆写）。
const SUBTITLE_OFFSET: f32 = 128.0;

/// 各实体的 `Name` 组件值（与 intro.scene.json 一致）。
const ENTITY_OVERLAY: &str = "intro_overlay";
const ENTITY_TITLE: &str = "intro_title";
const ENTITY_SUBTITLE: &str = "intro_subtitle";
const ENTITY_PROMPT: &str = "intro_prompt";

// ---------------------------------------------------------------------------
// 雨滴（项目自带的极简 2D 粒子）
// ---------------------------------------------------------------------------

const MAX_DROPS: usize = 192;
const SPAWN_PER_SECOND: f32 = 140.0;
/// 雨滴绘制层级：高于 UI 但低于调试叠加。
const DROP_LAYER: i32 = 900;

struct RainDrop {
    x: f32,
    y: f32,
    speed: f32,
    length: f32,
}

/// 开屏雨滴。
///
/// 以前用引擎的 `ParticleSystem2D` 资源；插件形态不能链接引擎，所以这里自带
/// 一个几十行的发射器，通过 `GameWorld::draw_quad` 输出。引擎不需要知道雨。
struct Rain {
    drops: Vec<RainDrop>,
    carry: f32,
    seed: u32,
}

impl Rain {
    fn new() -> Self {
        Self {
            drops: Vec::new(),
            carry: 0.0,
            seed: 0x9E37_79B9,
        }
    }

    /// 线性同余，够用且不引入依赖；同一 seed 序列稳定。
    fn rand(&mut self) -> f32 {
        self.seed = self
            .seed
            .wrapping_mul(1_664_525)
            .wrapping_add(1_013_904_223);
        (self.seed >> 8) as f32 / 16_777_216.0
    }

    fn update(&mut self, dt: f32, bounds: [f32; 2], intensity: f32) {
        let (width, height) = (bounds[0].max(1.0), bounds[1].max(1.0));

        self.carry = (self.carry + SPAWN_PER_SECOND * intensity.clamp(0.0, 1.0) * dt).min(16.0);
        while self.carry >= 1.0 && self.drops.len() < MAX_DROPS {
            self.carry -= 1.0;
            let x = self.rand() * width;
            let y = -20.0 - self.rand() * height * 0.25;
            let speed = 900.0 + self.rand() * 700.0;
            let length = 18.0 + self.rand() * 26.0;
            self.drops.push(RainDrop {
                x,
                y,
                speed,
                length,
            });
        }

        for drop in &mut self.drops {
            drop.y += drop.speed * dt;
            drop.x -= drop.speed * 0.12 * dt;
        }
        self.drops
            .retain(|drop| drop.y < height + 40.0 && drop.x > -40.0);
    }

    fn draw(&self, world: &mut GameWorld<'_>, alpha: f32) {
        let alpha = alpha.clamp(0.0, 1.0);
        if alpha <= 0.0 {
            return;
        }
        for drop in &self.drops {
            world.draw_quad(UiQuad {
                rect: [drop.x, drop.y, 1.5, drop.length],
                color: [0.75, 0.85, 1.0, 0.35 * alpha],
                border_radius: 1.0,
                layer: DROP_LAYER,
            });
        }
    }
}

// ---------------------------------------------------------------------------
// 动画状态
// ---------------------------------------------------------------------------

/// intro 动画状态——**项目私有**，不进入引擎或编辑器。
pub struct IntroState {
    /// 全屏黑场 alpha（1 = 全黑）。
    overlay: AnimationSequence<f32>,
    /// 标题不透明度。
    title_opacity: AnimationSequence<f32>,
    /// 标题纵向位移（px，>0 在下方，动画归零上浮）。
    title_y: AnimationSequence<f32>,
    /// 跳过请求（`tick` 里置位并消费）。
    skip: bool,
    rain: Rain,
    overlay_entity: EntityHandle,
    title_entity: EntityHandle,
    subtitle_entity: EntityHandle,
    prompt_entity: EntityHandle,
}

impl IntroState {
    /// 推进 intro 动画（跳过时快进到结尾）。
    fn tick(&mut self, dt: f32) {
        if self.skip {
            self.skip = false;
            self.overlay.advance_to(self.overlay.duration());
            self.title_opacity.advance_to(self.title_opacity.duration());
            self.title_y.advance_to(self.title_y.duration());
            return;
        }
        if self.overlay.finished() {
            return;
        }
        self.overlay.advance_by(dt as f64);
        self.title_opacity.advance_by(dt as f64);
        self.title_y.advance_by(dt as f64);
    }
}

/// 检查缓存的场景实体句柄是否仍属于当前场景。
fn state_is_valid(world: &GameWorld<'_>, state: &IntroState) -> bool {
    let entities = [
        state.overlay_entity,
        state.title_entity,
        state.subtitle_entity,
        state.prompt_entity,
    ];
    entities.iter().all(|&entity| world.is_alive(entity))
        && world.get::<Style>(state.overlay_entity).is_some()
        && world.get::<Style>(state.title_entity).is_some()
        && world.get::<Style>(state.subtitle_entity).is_some()
        && world.get::<Text>(state.title_entity).is_some()
        && world.get::<Text>(state.subtitle_entity).is_some()
        && world.get::<Text>(state.prompt_entity).is_some()
}

/// 在第一帧按 Name 查找场景实体，构造 IntroState。
fn initialize_intro(world: &GameWorld<'_>) -> Option<IntroState> {
    let overlay_entity = world.find_by_name(ENTITY_OVERLAY)?;
    let title_entity = world.find_by_name(ENTITY_TITLE)?;
    let subtitle_entity = world.find_by_name(ENTITY_SUBTITLE)?;
    let prompt_entity = world.find_by_name(ENTITY_PROMPT)?;

    log::info!("intro: found all 4 UI entities");

    let fade_in = 0.9;
    let hold = 1.6;
    let fade_out = 0.6;
    let hold_end = fade_in + hold;

    let overlay = keyframes![
        (1.0, 0.0, EaseInOutCubic),
        (0.0, fade_in),
        (0.0, hold_end + fade_out)
    ];
    let title_opacity = keyframes![
        (0.0, 0.0, EaseOutCubic),
        (1.0, fade_in),
        (1.0, hold_end, EaseInCubic),
        (0.0, hold_end + fade_out)
    ];
    let title_y = keyframes![
        (24.0, 0.0, EaseOutCubic),
        (0.0, fade_in),
        (0.0, hold_end, EaseInCubic),
        (-24.0, hold_end + fade_out)
    ];

    Some(IntroState {
        overlay,
        title_opacity,
        title_y,
        skip: false,
        rain: Rain::new(),
        overlay_entity,
        title_entity,
        subtitle_entity,
        prompt_entity,
    })
}

/// 任意键或左键点击都跳过 intro。
fn should_skip(input: &InputSnapshot) -> bool {
    input.left_clicked || input.any_key()
}

/// 推进 intro 动画并写回 UI 组件。
///
/// 第一帧按 `Name` 组件查找实体、创建 [`IntroState`]。`state` 为 `None` 时
/// 表示还没绑定（场景实体未就绪，或场景重载后句柄失效）。
pub fn tick(state: &mut Option<IntroState>, world: &mut GameWorld<'_>, dt: f32) {
    if state
        .as_ref()
        .is_some_and(|current| !state_is_valid(world, current))
    {
        *state = None;
    }

    // 1. 惰性初始化：第一帧按 Name 查找实体。
    if state.is_none() {
        match initialize_intro(world) {
            Some(created) => *state = Some(created),
            None => return,
        }
    }

    // 2. 跳过检测（引擎每帧下发的输入快照）。
    let skip = should_skip(&world.input());

    // 3. 推进动画、取出本帧要写回的动画值与受控实体。
    let (
        overlay_alpha,
        title_alpha,
        title_y,
        prompt_alpha,
        overlay_e,
        title_e,
        subtitle_e,
        prompt_e,
    ) = {
        let Some(current) = state.as_mut() else {
            return;
        };
        if skip {
            current.skip = true;
        }
        current.tick(dt);
        let title_alpha = current.title_opacity.now();
        let time = current.overlay.time() as f32;
        let pulse = 0.35 + 0.35 * ((time * 4.0).sin() * 0.5 + 0.5);
        (
            current.overlay.now(),
            title_alpha,
            current.title_y.now(),
            title_alpha * pulse,
            current.overlay_entity,
            current.title_entity,
            current.subtitle_entity,
            current.prompt_entity,
        )
    };

    // 4. 开屏雨滴：强度跟随标题淡入淡出，提交为 UI 四边形。
    let screen = world.screen_size();
    if let Some(current) = state.as_mut() {
        current.rain.update(dt, screen, title_alpha);
        current.rain.draw(world, title_alpha);
    }

    // 5. 写回组件（读-改-写：插件形态下没有 `&mut` 借用）。
    if let Some(mut style) = world.get::<Style>(overlay_e) {
        style.background[3] = overlay_alpha;
        world.set(overlay_e, style);
    }
    if let Some(mut style) = world.get::<Style>(title_e) {
        style.margin.top = TITLE_BASE_MARGIN + title_y;
        world.set(title_e, style);
    }
    if let Some(mut text) = world.get::<Text>(title_e) {
        text.color[3] = title_alpha;
        world.set(title_e, text);
    }
    if let Some(mut style) = world.get::<Style>(subtitle_e) {
        style.margin.top = TITLE_BASE_MARGIN + SUBTITLE_OFFSET + title_y;
        world.set(subtitle_e, style);
    }
    if let Some(mut text) = world.get::<Text>(subtitle_e) {
        text.color[3] = title_alpha;
        world.set(subtitle_e, text);
    }
    if let Some(mut text) = world.get::<Text>(prompt_e) {
        text.color[3] = prompt_alpha;
        world.set(prompt_e, text);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        initialize_intro, should_skip, state_is_valid, ENTITY_OVERLAY, ENTITY_PROMPT,
        ENTITY_SUBTITLE, ENTITY_TITLE,
    };
    use prism_engine::ecs::{Entity, World};
    use prism_game_api::{GameWorld, InputSnapshot};
    use prism_scene::components::Name;
    use prism_scene::ui::{Style, Text};

    fn input(any_key: bool, clicked: bool) -> InputSnapshot {
        let mut input = InputSnapshot::default();
        if any_key {
            input.set_key(0, true);
        }
        input.left_clicked = clicked;
        input
    }

    #[test]
    fn any_key_skips() {
        assert!(should_skip(&input(true, false)));
    }

    #[test]
    fn click_skips() {
        assert!(should_skip(&input(false, true)));
    }

    #[test]
    fn idle_input_does_not_skip() {
        assert!(!should_skip(&input(false, false)));
    }

    #[test]
    fn tick_writes_animation_values_through_game_world() {
        let mut world = World::new();
        for name in [ENTITY_OVERLAY, ENTITY_TITLE, ENTITY_SUBTITLE, ENTITY_PROMPT] {
            let entity = world.spawn();
            world.insert(entity, Name(name.to_owned()));
            world.insert(entity, Style::default());
            if name != ENTITY_OVERLAY {
                world.insert(entity, Text::default());
            }
        }

        let mut state = None;
        {
            let mut api = GameWorld::host(&mut world);
            super::tick(&mut state, &mut api, 1.0 / 60.0);
        }
        let state = state.expect("intro binds on first tick");

        // 黑场从全黑开始淡出，标题同时上浮：两者都应已经写进组件。
        let overlay = world
            .get::<Style>(Entity::from_raw(
                state.overlay_entity.id,
                state.overlay_entity.generation,
            ))
            .expect("overlay style");
        assert!(overlay.background[3] > 0.0);

        let title = world
            .get::<Style>(Entity::from_raw(
                state.title_entity.id,
                state.title_entity.generation,
            ))
            .expect("title style");
        assert_ne!(title.margin.top, Style::default().margin.top);
    }

    #[test]
    fn cached_intro_handles_are_invalidated_after_scene_entities_are_replaced() {
        let mut world = World::new();
        let names = [ENTITY_OVERLAY, ENTITY_TITLE, ENTITY_SUBTITLE, ENTITY_PROMPT];
        for name in names {
            let entity = world.spawn();
            world.insert(entity, Name(name.to_owned()));
            world.insert(entity, Style::default());
            if name != ENTITY_OVERLAY {
                world.insert(entity, Text::default());
            }
        }

        let state = {
            let api = GameWorld::host(&mut world);
            initialize_intro(&api).expect("scene entities should bind")
        };
        {
            let api = GameWorld::host(&mut world);
            assert!(state_is_valid(&api, &state));
        }

        world.despawn(Entity::from_raw(
            state.title_entity.id,
            state.title_entity.generation,
        ));
        let api = GameWorld::host(&mut world);
        assert!(!state_is_valid(&api, &state));
    }
}
