//! Render-only interpolation for transforms driven by fixed-step simulation.

use prism_ecs::{Entity, World};
use prism_scene::components::{
    Child, LocalToWorld, LocalTransform, Parent, TransformInterpolation2D,
};

/// Move the last fixed-step sample forward before simulation runs.
pub fn begin_fixed_step(world: &mut World) {
    let entities: Vec<_> = world
        .query::<TransformInterpolation2D>()
        .map(|(entity, _)| entity)
        .collect();
    for entity in entities {
        let Some(transform) = world.get::<LocalTransform>(entity) else {
            continue;
        };
        let position = [transform.position.x, transform.position.y];
        if let Some(history) = world.get_mut::<TransformInterpolation2D>(entity) {
            history.previous = history.current;
            history.current = position;
        }
    }
}

/// Store the post-simulation local position as the current sample.
pub fn end_fixed_step(world: &mut World) {
    let entities: Vec<_> = world
        .query::<TransformInterpolation2D>()
        .map(|(entity, _)| entity)
        .collect();
    for entity in entities {
        let Some(transform) = world.get::<LocalTransform>(entity) else {
            continue;
        };
        let position = [transform.position.x, transform.position.y];
        if let Some(history) = world.get_mut::<TransformInterpolation2D>(entity) {
            history.current = position;
        }
    }
}

/// Apply interpolated matrices for rendering and virtual-camera target reads.
pub fn apply_render_interpolation(world: &mut World, alpha: f32) {
    let entities: Vec<_> = world
        .query::<LocalToWorld>()
        .map(|(entity, _)| entity)
        .collect();
    for entity in entities {
        let Some(matrix) = render_world_matrix(world, entity, alpha, &mut Vec::new()) else {
            continue;
        };
        if let Some(world_transform) = world.get_mut::<LocalToWorld>(entity) {
            world_transform.0 = matrix;
        }
    }
}

fn render_world_matrix(
    world: &World,
    entity: Entity,
    alpha: f32,
    visiting: &mut Vec<Entity>,
) -> Option<glam::Mat4> {
    if !world.is_alive(entity) || visiting.contains(&entity) {
        return None;
    }
    visiting.push(entity);

    let parent = world
        .get::<Parent>(entity)
        .map(|parent| parent.0)
        .filter(|parent| {
            world.is_alive(*parent)
                && world
                    .get::<Child>(*parent)
                    .is_some_and(|children| children.0.contains(&entity))
        });
    let parent_matrix = match parent {
        Some(parent) => render_world_matrix(world, parent, alpha, visiting)?,
        None => glam::Mat4::IDENTITY,
    };

    let matrix = if let Some(mut local) = world.get::<LocalTransform>(entity).cloned() {
        if let Some(history) = world.get::<TransformInterpolation2D>(entity) {
            let position = history.sample(alpha);
            local.position.x = position[0];
            local.position.y = position[1];
        }
        parent_matrix * local.to_model_matrix()
    } else if parent.is_some() {
        parent_matrix
    } else {
        world.get::<LocalToWorld>(entity)?.0
    };

    visiting.pop();
    matrix.is_finite().then_some(matrix)
}

#[cfg(test)]
mod tests {
    use super::*;
    use prism_scene::components::{Child, Parent};

    #[test]
    fn interpolation_changes_only_the_render_matrix() {
        let mut world = World::new();
        let entity = world.spawn();
        world.insert(
            entity,
            LocalTransform {
                position: glam::Vec3::new(10.0, 4.0, 0.0),
                ..LocalTransform::default()
            },
        );
        world.insert(
            entity,
            TransformInterpolation2D {
                previous: [2.0, 0.0],
                current: [10.0, 4.0],
            },
        );
        world.insert(entity, LocalToWorld(glam::Mat4::IDENTITY));

        apply_render_interpolation(&mut world, 0.25);

        let rendered = world.get::<LocalToWorld>(entity).unwrap().0.w_axis;
        assert_eq!(rendered, glam::Vec4::new(4.0, 1.0, 0.0, 1.0));
        assert_eq!(
            world.get::<LocalTransform>(entity).unwrap().position.x,
            10.0
        );
    }

    #[test]
    fn child_render_transform_inherits_parent_interpolation() {
        let mut world = World::new();
        let parent = world.spawn();
        let child = world.spawn();
        world.insert(
            parent,
            LocalTransform {
                position: glam::Vec3::new(10.0, 0.0, 0.0),
                ..LocalTransform::default()
            },
        );
        world.insert(
            parent,
            TransformInterpolation2D {
                previous: [2.0, 0.0],
                current: [10.0, 0.0],
            },
        );
        world.insert(parent, Child(vec![child]));
        world.insert(child, Parent(parent));
        world.insert(
            child,
            LocalTransform {
                position: glam::Vec3::new(0.0, 3.0, 0.0),
                ..LocalTransform::default()
            },
        );
        world.insert(child, LocalToWorld(glam::Mat4::IDENTITY));

        apply_render_interpolation(&mut world, 0.5);

        let rendered = world.get::<LocalToWorld>(child).unwrap().0.w_axis;
        assert_eq!(rendered.x, 6.0);
        assert_eq!(rendered.y, 3.0);
    }

    #[test]
    fn fixed_step_advances_local_history() {
        let mut world = World::new();
        let entity = world.spawn();
        world.insert(entity, LocalTransform::default());
        world.insert(
            entity,
            TransformInterpolation2D {
                previous: [0.0, 0.0],
                current: [0.0, 0.0],
            },
        );

        begin_fixed_step(&mut world);
        world.get_mut::<LocalTransform>(entity).unwrap().position.x = 5.0;
        end_fixed_step(&mut world);

        let history = world.get::<TransformInterpolation2D>(entity).unwrap();
        assert_eq!(history.previous, [0.0, 0.0]);
        assert_eq!(history.current, [5.0, 0.0]);
    }
}
