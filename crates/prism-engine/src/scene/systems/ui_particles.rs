//! Screen-space (UI layer) CPU particles (#1057).
//!
//! The author component is [`UiParticleEmitter`]; live particles stay in
//! [`UiParticleEmitterState`], which is never serialized. Every tick the system
//! advances the simulation in pixel space and appends plain quads to
//! `ExternalUiQuads`. `ui_render_system` merges that resource into the ordered
//! `UiDrawList` later in the same tick, so particles draw above UI panels
//! (`layer`) without a new pass or shader.
//!
//! Motivation: `ParticleEmitter2D` integrates in world XY and draws through the
//! sprite channel, where a full-screen UI panel covers it. A fade-in black
//! screen (or a menu backdrop) would hide rain / snow that is supposed to be on
//! top of it.

use prism_ecs::{Entity, World};

use crate::scene::components::UiParticleEmitter;
use crate::scene::systems::hierarchy::is_active_in_hierarchy;
use crate::ui::{ExternalUiQuads, ParticleSystem2D, ScreenSize};

/// Screen used before the first layout pass publishes `ScreenSize`.
const FALLBACK_WIDTH: f32 = 1920.0;
const FALLBACK_HEIGHT: f32 = 1080.0;

/// Live particles plus simulation state for one [`UiParticleEmitter`].
#[derive(Debug)]
pub struct UiParticleEmitterState {
    pub system: ParticleSystem2D,
}

impl UiParticleEmitterState {
    /// Empty pool seeded from the entity id.
    ///
    /// Without a per-entity seed every instance of a preset would spawn at the
    /// same positions; the id keeps two emitters visually independent while
    /// still reproducing the same pattern for a scene loaded again.
    pub fn seeded(entity: Entity) -> Self {
        let mut system = ParticleSystem2D::new();
        system.set_seed((entity.id() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        Self { system }
    }
}

/// Advance every active [`UiParticleEmitter`] and queue its quads for the UI
/// pass. Returns the number of live particles (editor / tests).
pub fn ui_particle_emitter_system(world: &mut World, dt: f32) -> usize {
    if !dt.is_finite() || dt <= 0.0 {
        return 0;
    }

    let screen = world
        .get_resource::<ScreenSize>()
        .copied()
        .unwrap_or(ScreenSize {
            width: FALLBACK_WIDTH,
            height: FALLBACK_HEIGHT,
        });
    let width = if screen.width.is_finite() && screen.width > 0.0 {
        screen.width
    } else {
        FALLBACK_WIDTH
    };
    let height = if screen.height.is_finite() && screen.height > 0.0 {
        screen.height
    } else {
        FALLBACK_HEIGHT
    };

    let entities: Vec<Entity> = world
        .query::<UiParticleEmitter>()
        .filter(|(entity, _)| is_active_in_hierarchy(world, *entity))
        .map(|(entity, _)| entity)
        .collect();

    let mut alive = 0usize;
    for entity in entities {
        let Some(emitter_def) = world.get::<UiParticleEmitter>(entity).cloned() else {
            continue;
        };
        if world.get::<UiParticleEmitterState>(entity).is_none() {
            world.insert(entity, UiParticleEmitterState::seeded(entity));
        }
        let Some(mut state) = world.remove::<UiParticleEmitterState>(entity) else {
            continue;
        };

        state.system.apply_emitter(&emitter_def);
        state.system.set_bounds(width, height);
        // `playing = false` only stops spawning; live particles still finish.
        state.system.intensity = if emitter_def.playing {
            emitter_def.clamped_intensity()
        } else {
            0.0
        };
        state.system.update(dt);
        alive += state.system.particles.len();

        let quads = state.system.emit_quads();
        world.insert(entity, state);

        if quads.is_empty() {
            continue;
        }
        if let Some(queued) = world.get_resource_mut::<ExternalUiQuads>() {
            queued.0.extend(quads);
        } else {
            world.insert_resource(ExternalUiQuads(quads));
        }
    }
    alive
}

#[cfg(test)]
#[path = "ui_particles_tests.rs"]
mod tests;
