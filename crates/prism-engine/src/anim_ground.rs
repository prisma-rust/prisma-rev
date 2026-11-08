//! Ground queries for foot placement (#560 P4).
//!
//! `prism-anim-ik` never touches the ECS or the renderer: it asks a
//! [`GroundQuery`] "what is under this point" and the caller decides where the
//! answer comes from. This adapter answers from the authored [`Collider`]s
//! through [`crate::physics::raycast_excluding`], so foot IK works in a project
//! without the rapier `physics` feature and without a physics stepping order.
//!
//! Triggers are skipped on purpose: a sensor volume is not a surface a foot can
//! stand on.

use glam::Vec3;
use prism_anim_ik::foot::{GroundHit, GroundQuery};
use prism_ecs::{Entity, World};

use crate::physics;

/// Answers foot placement probes from the scene's colliders.
///
/// The caster's own collider must be excluded (see [`Self::excluding`]) —
/// otherwise every probe from inside the character hits the character.
pub struct SceneGroundQuery<'a> {
    world: &'a World,
    layer_mask: u32,
    excluded: Option<Entity>,
}

impl<'a> SceneGroundQuery<'a> {
    /// Query colliders whose `layer` overlaps `layer_mask`.
    ///
    /// Pass `u32::MAX` to consider every collider, or a dedicated ground layer so
    /// character capsules, props and triggers are ignored by construction.
    pub fn new(world: &'a World, layer_mask: u32) -> Self {
        Self {
            world,
            layer_mask,
            excluded: None,
        }
    }

    /// Skip the character's own colliders.
    pub fn excluding(mut self, entity: Entity) -> Self {
        self.excluded = Some(entity);
        self
    }
}

impl GroundQuery for SceneGroundQuery<'_> {
    fn cast(&self, origin: Vec3, direction: Vec3, max_distance: f32) -> Option<GroundHit> {
        if !origin.is_finite() || !direction.is_finite() || !(max_distance > 0.0) {
            return None;
        }
        let hit = match self.excluded {
            Some(entity) => physics::raycast_excluding(
                self.world,
                origin.to_array(),
                direction.normalize_or_zero().to_array(),
                max_distance,
                self.layer_mask,
                entity,
            ),
            None => physics::raycast(
                self.world,
                origin.to_array(),
                direction.normalize_or_zero().to_array(),
                max_distance,
                self.layer_mask,
            ),
        }?;
        if hit.is_trigger {
            return None;
        }
        Some(GroundHit {
            point: Vec3::from_array(hit.point),
            normal: Vec3::from_array(hit.normal).normalize_or_zero(),
        })
    }
}

#[cfg(test)]
mod tests {
    use prism_ecs::World;
    use prism_scene::components::{Collider, ColliderShape, LocalToWorld, LocalTransform};

    use super::SceneGroundQuery;
    use prism_anim_ik::foot::GroundQuery;

    fn spawn_box(
        world: &mut World,
        translation: [f32; 3],
        half: [f32; 3],
        layer: u32,
    ) -> prism_ecs::Entity {
        let entity = world.spawn();
        world.insert(
            entity,
            Collider {
                shape: ColliderShape::Box,
                half_extents: half,
                layer,
                ..Collider::default()
            },
        );
        world.insert(
            entity,
            LocalTransform {
                position: glam::Vec3::from_array(translation),
                ..LocalTransform::default()
            },
        );
        world.insert(
            entity,
            LocalToWorld(glam::Mat4::from_translation(glam::Vec3::from_array(
                translation,
            ))),
        );
        entity
    }

    #[test]
    fn cast_finds_the_floor_below_the_ankle() {
        let mut world = World::new();
        spawn_box(&mut world, [0.0, -0.5, 0.0], [10.0, 0.5, 10.0], 1);
        let query = SceneGroundQuery::new(&world, u32::MAX);
        let hit = query
            .cast(glam::Vec3::new(1.0, 0.4, -2.0), glam::Vec3::NEG_Y, 1.0)
            .expect("floor under the ankle");
        assert!((hit.point.y - 0.0).abs() < 1e-4, "{:?}", hit.point);
        assert!(hit.normal.y > 0.9, "{:?}", hit.normal);
    }

    #[test]
    fn excluding_the_caster_ignores_its_own_collider() {
        let mut world = World::new();
        let character = spawn_box(&mut world, [0.0, 0.45, 0.0], [0.3, 0.45, 0.3], 1);
        spawn_box(&mut world, [0.0, -0.1, 0.0], [10.0, 0.1, 10.0], 1);
        let origin = glam::Vec3::new(0.0, 0.3, 0.0);
        let plain = SceneGroundQuery::new(&world, u32::MAX);
        let without_caster = SceneGroundQuery::new(&world, u32::MAX).excluding(character);
        // The probe starts inside the capsule, so the caster is the first hit.
        let self_hit = plain
            .cast(origin, glam::Vec3::NEG_Y, 2.0)
            .expect("the caster is hit");
        assert!(self_hit.point.y > 0.2, "{:?}", self_hit.point);
        let hit = without_caster
            .cast(origin, glam::Vec3::NEG_Y, 2.0)
            .expect("floor under the capsule");
        assert!((hit.point.y - 0.0).abs() < 1e-4, "{:?}", hit.point);
    }

    #[test]
    fn layer_mask_keeps_props_out_of_foot_probes() {
        let mut world = World::new();
        spawn_box(&mut world, [0.0, -0.1, 0.0], [10.0, 0.1, 10.0], 1);
        spawn_box(&mut world, [0.0, 0.2, 0.0], [1.0, 0.1, 1.0], 2);
        let ground_only = SceneGroundQuery::new(&world, 1);
        let hit = ground_only
            .cast(glam::Vec3::new(0.0, 0.5, 0.0), glam::Vec3::NEG_Y, 2.0)
            .expect("floor behind the crate");
        assert!((hit.point.y - 0.0).abs() < 1e-4, "{:?}", hit.point);
    }

    #[test]
    fn triggers_are_not_ground() {
        let mut world = World::new();
        let entity = spawn_box(&mut world, [0.0, -0.1, 0.0], [10.0, 0.1, 10.0], 1);
        if let Some(collider) = world.get_mut::<Collider>(entity) {
            collider.is_trigger = true;
        }
        let query = SceneGroundQuery::new(&world, u32::MAX);
        assert_eq!(
            query.cast(glam::Vec3::new(0.0, 0.5, 0.0), glam::Vec3::NEG_Y, 2.0),
            None
        );
    }
}
