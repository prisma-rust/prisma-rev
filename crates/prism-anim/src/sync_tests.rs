use super::{SyncGroup, SyncMarkerSet};

fn markers(phases: &[f32]) -> SyncMarkerSet {
    SyncMarkerSet::new(phases.iter().copied()).expect("markers")
}

fn group(leader: &[f32], follower: &[f32]) -> SyncGroup {
    SyncGroup::new(vec![markers(leader), markers(follower)]).expect("group")
}

fn close(a: f32, b: f32) -> bool {
    (a - b).abs() <= 1e-5
}

#[test]
fn an_identity_group_maps_every_phase_to_itself() {
    let group = group(&[0.0, 0.5], &[0.0, 0.5]);
    for phase in [0.0, 0.1, 0.25, 0.5, 0.9] {
        assert!(close(group.map_phase(0, 1, phase), phase), "{phase}");
        assert!(close(group.map_phase(1, 0, phase), phase), "{phase}");
    }
}

#[test]
fn markers_are_sorted_normalized_and_deduplicated() {
    let markers = SyncMarkerSet::new([1.25, -0.25, 0.5, 0.5, f32::NAN]).unwrap();
    assert_eq!(markers.phases(), &[0.25, 0.5, 0.75]);
    assert!(markers.phases().windows(2).all(|pair| pair[0] < pair[1]));
    assert!(SyncMarkerSet::new([f32::NAN, f32::INFINITY]).is_none());
}

#[test]
fn a_marker_offset_shifts_the_follower_segment() {
    let group = group(&[0.0, 0.5], &[0.25, 0.75]);
    assert!(close(group.map_phase(0, 1, 0.0), 0.25));
    assert!(close(group.map_phase(0, 1, 0.25), 0.5));
    assert!(close(group.map_phase(0, 1, 0.5), 0.75));
    assert!(close(group.map_phase(0, 1, 0.75), 0.0));
    assert!(close(group.map_phase(0, 1, 0.9), 0.15));
}

#[test]
fn phases_before_the_first_marker_use_the_wrapped_segment() {
    // leader 段：0.75 -> 1.25（即 0.25）；phase 0.1 位于该段的 70%。
    let group = group(&[0.25, 0.75], &[0.1, 0.6]);
    let mapped = group.map_phase(0, 1, 0.1);
    assert!(close(mapped, 0.95), "mapped {mapped}");
}

#[test]
fn a_single_marker_pins_the_follower() {
    let group = group(&[0.3], &[0.7]);
    assert!(close(group.map_phase(0, 1, 0.0), 0.7));
    assert!(close(group.map_phase(0, 1, 0.6), 0.7));
    assert!(close(group.map_phase(1, 0, 0.0), 0.3));
}

#[test]
fn groups_require_matching_marker_counts() {
    assert!(SyncGroup::new(vec![]).is_none());
    assert!(SyncGroup::new(vec![markers(&[0.0, 0.5])]).is_some());
    assert!(SyncGroup::new(vec![markers(&[0.0, 0.5]), markers(&[0.0])]).is_none());
}

#[test]
fn bad_indices_and_phases_fall_back_to_the_input_phase() {
    let group = group(&[0.0, 0.5], &[0.25, 0.75]);
    assert_eq!(group.map_phase(0, 99, 0.25), 0.25);
    assert_eq!(group.map_phase(99, 0, 0.25), 0.25);
    assert!(group.map_phase(0, 1, f32::NAN).is_nan());
}

#[test]
fn negative_and_over_one_phases_are_normalized() {
    let group = group(&[0.0, 0.5], &[0.25, 0.75]);
    assert!(close(
        group.map_phase(0, 1, -0.5),
        group.map_phase(0, 1, 0.5)
    ));
    assert!(close(
        group.map_phase(0, 1, 1.25),
        group.map_phase(0, 1, 0.25)
    ));
}
