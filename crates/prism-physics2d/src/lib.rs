//! Deterministic 2D collision geometry for platform games.
//!
//! This crate is deliberately independent of ECS, rendering, and a rigid-body
//! backend. The XY plane uses +X right and +Y up; callers choose the world unit.

#![deny(warnings)]

const EPSILON: f32 = 1.0e-6;
const MAX_CONTACTS: usize = 4;
const MAX_DEPENETRATION_STEPS: usize = 8;

/// An axis-aligned rectangle in world space.
///
/// Touching edges do not overlap. `min` and `max` are expected to be ordered;
/// constructors normalize them so authoring data cannot silently invert a box.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Aabb2 {
    pub min: [f32; 2],
    pub max: [f32; 2],
}

impl Aabb2 {
    pub const fn new(x0: f32, y0: f32, x1: f32, y1: f32) -> Self {
        Self {
            min: [if x0 < x1 { x0 } else { x1 }, if y0 < y1 { y0 } else { y1 }],
            max: [if x0 > x1 { x0 } else { x1 }, if y0 > y1 { y0 } else { y1 }],
        }
    }

    pub fn from_center_half_extents(center: [f32; 2], half_extents: [f32; 2]) -> Self {
        Self::new(
            center[0] - half_extents[0].abs(),
            center[1] - half_extents[1].abs(),
            center[0] + half_extents[0].abs(),
            center[1] + half_extents[1].abs(),
        )
    }

    pub fn overlaps(self, other: Self) -> bool {
        self.min[0] < other.max[0]
            && self.max[0] > other.min[0]
            && self.min[1] < other.max[1]
            && self.max[1] > other.min[1]
    }

    pub fn contains_point(self, point: [f32; 2]) -> bool {
        point[0] > self.min[0]
            && point[0] < self.max[0]
            && point[1] > self.min[1]
            && point[1] < self.max[1]
    }
}

/// The first hit of a 2D ray against an AABB.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RayHit2 {
    /// Distance from the origin in the caller's world units.
    pub distance: f32,
    pub point: [f32; 2],
    /// Outward face normal; zero when the ray starts inside the box.
    pub normal: [f32; 2],
}

/// Cast a normalized 2D ray against one AABB and return its first hit.
///
/// A ray starting inside the box hits at distance zero. Zero directions,
/// non-finite inputs, and non-positive maximum distances return `None`.
pub fn raycast_aabb(
    origin: [f32; 2],
    direction: [f32; 2],
    max_distance: f32,
    bounds: Aabb2,
) -> Option<RayHit2> {
    if !finite2(origin)
        || !finite2(direction)
        || !finite_aabb(bounds)
        || !max_distance.is_finite()
        || max_distance <= 0.0
    {
        return None;
    }
    let length = (direction[0] * direction[0] + direction[1] * direction[1]).sqrt();
    if !length.is_finite() || length <= EPSILON {
        return None;
    }
    let normalized = [direction[0] / length, direction[1] / length];
    if bounds.contains_point(origin) {
        return Some(RayHit2 {
            distance: 0.0,
            point: origin,
            normal: [0.0, 0.0],
        });
    }
    let delta = [normalized[0] * max_distance, normalized[1] * max_distance];
    let (toi, normal) = sweep_point(origin, delta, bounds)?;
    let distance = toi * max_distance;
    Some(RayHit2 {
        distance,
        point: [
            origin[0] + normalized[0] * distance,
            origin[1] + normalized[1] * distance,
        ],
        normal,
    })
}

/// A contact discovered while sliding an AABB.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Contact2 {
    /// Outward normal of the obstacle at the contact point.
    pub normal: [f32; 2],
    /// Fraction of the remaining move at which this contact occurred.
    pub toi: f32,
}

/// Result of [`move_and_slide`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MoveResult2 {
    /// Center position after the complete sweep and slide.
    pub position: [f32; 2],
    /// Difference between the returned position and the start position.
    pub applied_delta: [f32; 2],
    /// Contacts in sweep order. At most four are retained; flags remain valid
    /// even when a corner produces more contacts than the fixed storage.
    pub contacts: [Contact2; MAX_CONTACTS],
    pub contact_count: usize,
    pub hit_x: bool,
    pub hit_y: bool,
    pub grounded: bool,
    pub ceilinged: bool,
}

/// Static obstacle metadata used by the platformer sweep.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Obstacle2D {
    pub bounds: Aabb2,
    /// A horizontal platform that only blocks movement from above.
    pub one_way_top: bool,
}

impl Obstacle2D {
    pub const fn solid(bounds: Aabb2) -> Self {
        Self {
            bounds,
            one_way_top: false,
        }
    }

    pub const fn one_way_top(bounds: Aabb2) -> Self {
        Self {
            bounds,
            one_way_top: true,
        }
    }
}

/// Per-sweep options for platformer-specific collision rules.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MoveOptions2D {
    /// Ignore one-way platforms for this simulation step.
    pub drop_through_one_way: bool,
}

impl MoveResult2 {
    fn new(start: [f32; 2]) -> Self {
        Self {
            position: start,
            applied_delta: [0.0, 0.0],
            contacts: [Contact2::default(); MAX_CONTACTS],
            contact_count: 0,
            hit_x: false,
            hit_y: false,
            grounded: false,
            ceilinged: false,
        }
    }

    fn push_contact(&mut self, normal: [f32; 2], toi: f32) {
        if self.contact_count < MAX_CONTACTS {
            self.contacts[self.contact_count] = Contact2 { normal, toi };
            self.contact_count += 1;
        }
        if normal[0].abs() > 0.5 {
            self.hit_x = true;
        }
        if normal[1].abs() > 0.5 {
            self.hit_y = true;
        }
        if normal[1] > 0.5 {
            self.grounded = true;
        } else if normal[1] < -0.5 {
            self.ceilinged = true;
        }
    }
}

/// Continuously sweep an AABB against static AABB obstacles and slide along
/// their surfaces.
///
/// `start` is the center position, `half_extents` is the moving box's half size,
/// and `delta` is the requested displacement for one simulation step. The
/// For a non-zero displacement the returned position never enters an obstacle.
/// A zero displacement is a pure query and does not depenetrate an
/// already-overlapping box.
pub fn move_and_slide(
    start: [f32; 2],
    half_extents: [f32; 2],
    delta: [f32; 2],
    obstacles: &[Aabb2],
) -> MoveResult2 {
    let obstacles: Vec<_> = obstacles.iter().copied().map(Obstacle2D::solid).collect();
    move_and_slide_with_options(
        start,
        half_extents,
        delta,
        &obstacles,
        MoveOptions2D::default(),
    )
}

/// Sweep an AABB against static obstacles with platformer collision options.
pub fn move_and_slide_with_options(
    start: [f32; 2],
    half_extents: [f32; 2],
    delta: [f32; 2],
    obstacles: &[Obstacle2D],
    options: MoveOptions2D,
) -> MoveResult2 {
    let half_extents = [half_extents[0].abs(), half_extents[1].abs()];
    let mut result = MoveResult2::new(start);
    if !finite2(start) || !finite2(half_extents) || !finite2(delta) {
        return result;
    }
    if delta[0].abs() <= EPSILON && delta[1].abs() <= EPSILON {
        return result;
    }

    depenetrate(&mut result, half_extents, obstacles);
    let mut remaining = delta;
    for _ in 0..MAX_CONTACTS {
        if remaining[0].abs() <= EPSILON && remaining[1].abs() <= EPSILON {
            break;
        }

        let mut best: Option<(f32, [f32; 2])> = None;
        for obstacle in obstacles.iter().copied() {
            if !finite_aabb(obstacle.bounds)
                || (obstacle.one_way_top && options.drop_through_one_way)
            {
                continue;
            }
            let hit = if obstacle.one_way_top {
                sweep_one_way_top(result.position, remaining, half_extents, obstacle.bounds)
            } else {
                let expanded = Aabb2::new(
                    obstacle.bounds.min[0] - half_extents[0],
                    obstacle.bounds.min[1] - half_extents[1],
                    obstacle.bounds.max[0] + half_extents[0],
                    obstacle.bounds.max[1] + half_extents[1],
                );
                sweep_point(result.position, remaining, expanded)
            };
            if let Some((toi, normal)) = hit {
                if best.is_none_or(|(best_toi, _)| toi < best_toi) {
                    best = Some((toi, normal));
                }
            }
        }

        let Some((toi, normal)) = best else {
            result.position[0] += remaining[0];
            result.position[1] += remaining[1];
            break;
        };

        let toi = toi.clamp(0.0, 1.0);
        result.position[0] += remaining[0] * toi;
        result.position[1] += remaining[1] * toi;
        result.push_contact(normal, toi);

        let remainder_scale = 1.0 - toi;
        remaining[0] *= remainder_scale;
        remaining[1] *= remainder_scale;
        let into_surface = remaining[0] * normal[0] + remaining[1] * normal[1];
        if into_surface < 0.0 {
            remaining[0] -= normal[0] * into_surface;
            remaining[1] -= normal[1] * into_surface;
        }

        if toi <= EPSILON && remaining[0].abs() <= EPSILON && remaining[1].abs() <= EPSILON {
            break;
        }
    }

    result.applied_delta = [result.position[0] - start[0], result.position[1] - start[1]];
    result
}

fn depenetrate(result: &mut MoveResult2, half_extents: [f32; 2], obstacles: &[Obstacle2D]) {
    for _ in 0..MAX_DEPENETRATION_STEPS {
        let moving = Aabb2::from_center_half_extents(result.position, half_extents);
        let mut best: Option<(f32, [f32; 2])> = None;
        for obstacle in obstacles.iter().copied() {
            if obstacle.one_way_top
                || !finite_aabb(obstacle.bounds)
                || !moving.overlaps(obstacle.bounds)
            {
                continue;
            }
            let candidates = [
                (moving.max[0] - obstacle.bounds.min[0], [-1.0, 0.0]),
                (obstacle.bounds.max[0] - moving.min[0], [1.0, 0.0]),
                (moving.max[1] - obstacle.bounds.min[1], [0.0, -1.0]),
                (obstacle.bounds.max[1] - moving.min[1], [0.0, 1.0]),
            ];
            let candidate = candidates
                .into_iter()
                .filter(|(depth, _)| *depth >= 0.0)
                .min_by(|a, b| a.0.total_cmp(&b.0));
            if let Some(candidate) = candidate {
                if best.is_none_or(|current| candidate.0 < current.0) {
                    best = Some(candidate);
                }
            }
        }

        let Some((depth, normal)) = best else {
            break;
        };
        let correction = depth + EPSILON * 2.0;
        result.position[0] += normal[0] * correction;
        result.position[1] += normal[1] * correction;
        result.push_contact(normal, 0.0);
    }
}

/// Sweep only against the top face of a horizontal one-way platform.
fn sweep_one_way_top(
    origin: [f32; 2],
    delta: [f32; 2],
    half_extents: [f32; 2],
    bounds: Aabb2,
) -> Option<(f32, [f32; 2])> {
    if delta[1] >= -EPSILON {
        return None;
    }
    let start_bottom = origin[1] - half_extents[1];
    if start_bottom < bounds.max[1] - EPSILON {
        return None;
    }
    let toi = (bounds.max[1] + half_extents[1] - origin[1]) / delta[1];
    if !(0.0..=1.0).contains(&toi) {
        return None;
    }
    let contact_x = origin[0] + delta[0] * toi;
    if contact_x + half_extents[0] <= bounds.min[0] || contact_x - half_extents[0] >= bounds.max[0]
    {
        return None;
    }
    Some((toi, [0.0, 1.0]))
}

/// Return the first contact between a moving point and an AABB.
fn sweep_point(origin: [f32; 2], delta: [f32; 2], bounds: Aabb2) -> Option<(f32, [f32; 2])> {
    if bounds.contains_point(origin) {
        let distances = [
            (origin[0] - bounds.min[0], [-1.0, 0.0]),
            (bounds.max[0] - origin[0], [1.0, 0.0]),
            (origin[1] - bounds.min[1], [0.0, -1.0]),
            (bounds.max[1] - origin[1], [0.0, 1.0]),
        ];
        return distances
            .into_iter()
            .filter(|(distance, _)| *distance >= 0.0)
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, normal)| (0.0, normal));
    }

    let mut enter: f32 = 0.0;
    let mut exit: f32 = 1.0;
    let mut normal = [0.0, 0.0];
    for axis in 0..2 {
        let origin_axis = origin[axis];
        let delta_axis = delta[axis];
        if delta_axis.abs() <= EPSILON {
            if origin_axis <= bounds.min[axis] || origin_axis >= bounds.max[axis] {
                return None;
            }
            continue;
        }

        let inverse = 1.0 / delta_axis;
        let mut near = (bounds.min[axis] - origin_axis) * inverse;
        let mut far = (bounds.max[axis] - origin_axis) * inverse;
        let near_normal = if delta_axis > 0.0 {
            let mut value = [0.0, 0.0];
            value[axis] = -1.0;
            value
        } else {
            let mut value = [0.0, 0.0];
            value[axis] = 1.0;
            value
        };
        if near > far {
            std::mem::swap(&mut near, &mut far);
        }
        if near > enter || (normal == [0.0, 0.0] && near >= enter) {
            enter = near;
            normal = near_normal;
        }
        exit = exit.min(far);
        if enter > exit {
            return None;
        }
    }

    if !(0.0..=1.0).contains(&enter) {
        return None;
    }
    if enter <= EPSILON && (delta[0] * normal[0] + delta[1] * normal[1]) >= 0.0 {
        return None;
    }
    Some((enter, normal))
}

fn finite2(value: [f32; 2]) -> bool {
    value[0].is_finite() && value[1].is_finite()
}

fn finite_aabb(value: Aabb2) -> bool {
    finite2(value.min)
        && finite2(value.max)
        && value.min[0] <= value.max[0]
        && value.min[1] <= value.max[1]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edges_touch_without_overlapping() {
        let left = Aabb2::new(0.0, 0.0, 10.0, 10.0);
        let right = Aabb2::new(10.0, 0.0, 20.0, 10.0);
        assert!(!left.overlaps(right));
    }

    #[test]
    fn high_speed_horizontal_move_stops_at_a_thin_wall() {
        let result = move_and_slide(
            [0.0, 5.0],
            [1.0, 1.0],
            [100.0, 0.0],
            &[Aabb2::new(10.0, 0.0, 11.0, 10.0)],
        );
        assert!((result.position[0] - 9.0).abs() < 1e-5);
        assert!(result.hit_x);
        assert_eq!(result.applied_delta, [9.0, 0.0]);
    }

    #[test]
    fn high_speed_leftward_move_stops_at_a_thin_wall() {
        let result = move_and_slide(
            [20.0, 5.0],
            [1.0, 1.0],
            [-100.0, 0.0],
            &[Aabb2::new(10.0, 0.0, 11.0, 10.0)],
        );
        assert!((result.position[0] - 12.0).abs() < 1e-5);
        assert!(result.hit_x);
        assert_eq!(result.applied_delta, [-8.0, 0.0]);
    }

    #[test]
    fn one_way_top_stops_a_falling_body_and_reports_grounded() {
        let platform = Obstacle2D::one_way_top(Aabb2::new(0.0, 0.0, 10.0, 2.0));
        let result = move_and_slide_with_options(
            [5.0, 10.0],
            [1.0, 1.0],
            [0.0, -20.0],
            &[platform],
            MoveOptions2D::default(),
        );

        assert!((result.position[1] - 3.0).abs() < 1e-5);
        assert!(result.grounded);
    }

    #[test]
    fn one_way_top_allows_upward_and_underside_movement() {
        let platform = Obstacle2D::one_way_top(Aabb2::new(0.0, 0.0, 10.0, 2.0));
        let upward = move_and_slide_with_options(
            [5.0, -5.0],
            [1.0, 1.0],
            [0.0, 10.0],
            &[platform],
            MoveOptions2D::default(),
        );
        let downward_from_below = move_and_slide_with_options(
            [5.0, 0.0],
            [1.0, 1.0],
            [0.0, -5.0],
            &[platform],
            MoveOptions2D::default(),
        );

        assert_eq!(upward.position, [5.0, 5.0]);
        assert_eq!(downward_from_below.position, [5.0, -5.0]);
        assert!(!upward.hit_y && !downward_from_below.hit_y);
    }

    #[test]
    fn one_way_top_does_not_block_its_sides_or_depenetrate() {
        let platform = Obstacle2D::one_way_top(Aabb2::new(0.0, 0.0, 10.0, 2.0));
        let side = move_and_slide_with_options(
            [-2.0, 1.0],
            [1.0, 0.5],
            [20.0, 0.0],
            &[platform],
            MoveOptions2D::default(),
        );
        let overlap = move_and_slide_with_options(
            [5.0, 2.0],
            [1.0, 1.0],
            [0.5, 0.0],
            &[platform],
            MoveOptions2D::default(),
        );

        assert_eq!(side.position, [18.0, 1.0]);
        assert_eq!(overlap.position, [5.5, 2.0]);
        assert!(!overlap.hit_y);
    }

    #[test]
    fn drop_through_ignores_one_way_but_keeps_solid_collision() {
        let platform = Obstacle2D::one_way_top(Aabb2::new(0.0, 0.0, 10.0, 2.0));
        let wall = Obstacle2D::solid(Aabb2::new(12.0, 0.0, 13.0, 12.0));
        let result = move_and_slide_with_options(
            [5.0, 10.0],
            [1.0, 1.0],
            [10.0, -10.0],
            &[platform, wall],
            MoveOptions2D {
                drop_through_one_way: true,
            },
        );

        assert!((result.position[0] - 11.0).abs() < 1e-5);
        assert!((result.position[1] - 0.0).abs() < 1e-5);
        assert!(result.hit_x);
        assert!(!result.grounded);
    }

    #[test]
    fn falling_move_reports_grounded_and_slides_horizontally() {
        let result = move_and_slide(
            [5.0, 10.0],
            [1.0, 1.0],
            [20.0, -20.0],
            &[Aabb2::new(0.0, 0.0, 100.0, 2.0)],
        );
        assert!((result.position[0] - 25.0).abs() < 1e-5);
        assert!((result.position[1] - 3.0).abs() < 1e-5);
        assert!(result.grounded);
    }

    #[test]
    fn corner_contact_slides_along_both_surfaces() {
        let result = move_and_slide(
            [5.0, 5.0],
            [1.0, 1.0],
            [20.0, 20.0],
            &[
                Aabb2::new(10.0, 0.0, 11.0, 20.0),
                Aabb2::new(0.0, 10.0, 20.0, 11.0),
            ],
        );
        assert!((result.position[0] - 9.0).abs() < 1e-5);
        assert!((result.position[1] - 9.0).abs() < 1e-5);
        assert!(result.hit_x && result.ceilinged);
    }

    #[test]
    fn zero_delta_does_not_push_an_embedded_box() {
        let start = [5.0, 5.0];
        let result = move_and_slide(
            start,
            [2.0, 2.0],
            [0.0, 0.0],
            &[Aabb2::new(0.0, 0.0, 10.0, 10.0)],
        );
        assert_eq!(result.position, start);
        assert_eq!(result.contact_count, 0);
    }

    #[test]
    fn nonzero_move_depenetrates_an_embedded_box() {
        let result = move_and_slide(
            [5.0, 5.0],
            [1.0, 1.0],
            [0.0, -1.0],
            &[Aabb2::new(0.0, 0.0, 10.0, 10.0)],
        );
        let body = Aabb2::from_center_half_extents(result.position, [1.0, 1.0]);
        assert!(!body.overlaps(Aabb2::new(0.0, 0.0, 10.0, 10.0)));
    }

    #[test]
    fn raycast_reports_distance_and_outward_normal() {
        let hit = raycast_aabb(
            [0.0, 5.0],
            [2.0, 0.0],
            20.0,
            Aabb2::new(10.0, 0.0, 12.0, 10.0),
        )
        .expect("ray hits the box");
        assert!((hit.distance - 10.0).abs() < 1e-5);
        assert_eq!(hit.point, [10.0, 5.0]);
        assert_eq!(hit.normal, [-1.0, 0.0]);
    }

    #[test]
    fn raycast_starting_inside_reports_zero_distance() {
        let hit = raycast_aabb(
            [5.0, 5.0],
            [1.0, 0.0],
            20.0,
            Aabb2::new(0.0, 0.0, 10.0, 10.0),
        )
        .expect("inside origin is a hit");
        assert_eq!(hit.distance, 0.0);
        assert_eq!(hit.point, [5.0, 5.0]);
        assert_eq!(hit.normal, [0.0, 0.0]);
    }
}
