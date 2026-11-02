use super::*;

/// `(from, to, min_state_fraction, duration, condition_count, 条件字节)`。
type RawTransition = (u32, u32, f32, f32, u32, Vec<u8>);

/// 手工拼一份 RASM（解码端测试专用：编码器在 `prism-cooker`，两 crate 不能互相依赖）。
fn build(states: &[(&str, &str, f32, bool)], transitions: &[RawTransition]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(RASM_MAGIC);
    out.push(RASM_VERSION);
    out.extend_from_slice(&(states.len() as u32).to_le_bytes());
    out.extend_from_slice(&(transitions.len() as u32).to_le_bytes());
    for (name, clip, speed, looped) in states {
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&(clip.len() as u16).to_le_bytes());
        out.extend_from_slice(clip.as_bytes());
        out.extend_from_slice(&speed.to_le_bytes());
        out.push(if *looped { RASM_STATE_LOOPED } else { 0 });
    }
    for (from, to, min_fraction, duration, condition_count, conditions) in transitions {
        out.extend_from_slice(&from.to_le_bytes());
        out.extend_from_slice(&to.to_le_bytes());
        out.extend_from_slice(&min_fraction.to_le_bytes());
        out.extend_from_slice(&duration.to_le_bytes());
        out.extend_from_slice(&condition_count.to_le_bytes());
        out.extend_from_slice(conditions);
    }
    out
}

fn trigger_condition(name: &str) -> Vec<u8> {
    let mut out = vec![CONDITION_TRIGGER];
    out.extend_from_slice(&(name.len() as u16).to_le_bytes());
    out.extend_from_slice(name.as_bytes());
    out
}

#[test]
fn a_well_formed_machine_decodes_with_names_and_indices() {
    let data = build(
        &[
            ("Idle", "char/idle.ranim", 1.0, true),
            ("Run", "char/run.ranim", 1.25, false),
        ],
        &[
            (0, 1, 0.25, 0.2, 1, {
                let mut condition = vec![CONDITION_FLOAT_GREATER];
                condition.extend_from_slice(&5u16.to_le_bytes());
                condition.extend_from_slice(b"speed");
                condition.extend_from_slice(&1.5f32.to_le_bytes());
                condition
            }),
            (u32::MAX, 0, 0.0, 0.0, 1, trigger_condition("Stop")),
        ],
    );

    let machine = decode_rasm(&data).expect("decodes");
    assert_eq!(machine.states.len(), 2);
    assert_eq!(machine.states[1].name, "Run");
    assert_eq!(machine.states[1].clip_path, "char/run.ranim");
    assert!((machine.states[1].speed - 1.25).abs() < 1e-6);
    assert!(!machine.states[1].looped);
    assert!(machine.states[0].looped);

    assert_eq!(machine.transitions[0].from, Some(0));
    assert_eq!(machine.transitions[0].to, 1);
    assert!((machine.transitions[0].min_state_fraction - 0.25).abs() < 1e-6);
    assert_eq!(
        machine.transitions[0].conditions,
        vec![TransitionConditionInfo::FloatGreater {
            name: "speed".into(),
            threshold: 1.5,
        }]
    );
    // u32::MAX = 任意状态。
    assert_eq!(machine.transitions[1].from, None);
    assert_eq!(
        machine.transitions[1].conditions,
        vec![TransitionConditionInfo::Trigger {
            name: "Stop".into()
        }]
    );
}

#[test]
fn every_condition_kind_round_trips_its_payload() {
    let mut bool_condition = vec![CONDITION_BOOL_IS];
    bool_condition.extend_from_slice(&4u16.to_le_bytes());
    bool_condition.extend_from_slice(b"fall");
    bool_condition.push(1);

    let mut less_condition = vec![CONDITION_FLOAT_LESS];
    less_condition.extend_from_slice(&5u16.to_le_bytes());
    less_condition.extend_from_slice(b"speed");
    less_condition.extend_from_slice(&0.5f32.to_le_bytes());

    let data = build(
        &[("A", "a.ranim", 1.0, true)],
        &[(
            u32::MAX,
            0,
            0.0,
            0.0,
            2,
            [bool_condition, less_condition].concat(),
        )],
    );
    let machine = decode_rasm(&data).expect("decodes");
    assert_eq!(
        machine.transitions[0].conditions,
        vec![
            TransitionConditionInfo::BoolIs {
                name: "fall".into(),
                expected: true,
            },
            TransitionConditionInfo::FloatLess {
                name: "speed".into(),
                threshold: 0.5,
            },
        ]
    );
}

#[test]
fn broken_files_are_rejected_instead_of_panicking() {
    let good = build(&[("A", "a.ranim", 1.0, true)], &[]);

    // 太短 / 错 magic / 错版本。
    assert!(decode_rasm(&[]).is_none());
    assert!(decode_rasm(&good[..12]).is_none());
    let mut wrong_magic = good.clone();
    wrong_magic[0] = b'X';
    assert!(decode_rasm(&wrong_magic).is_none());
    let mut wrong_version = good.clone();
    wrong_version[4] = 99;
    assert!(decode_rasm(&wrong_version).is_none());

    // 状态名字被截断、尾部多字节、状态数超大。
    let mut truncated = good.clone();
    truncated.truncate(good.len() - 1);
    assert!(decode_rasm(&truncated).is_none());
    let mut trailing = good.clone();
    trailing.push(0);
    assert!(decode_rasm(&trailing).is_none());
    let mut huge = good.clone();
    huge[5..9].copy_from_slice(&(MAX_STATES as u32 + 1).to_le_bytes());
    assert!(decode_rasm(&huge).is_none());

    // 未知条件种类 / 未知状态标志。
    let mut unknown_condition = vec![CONDITION_TRIGGER + 9];
    unknown_condition.extend_from_slice(&1u16.to_le_bytes());
    unknown_condition.extend_from_slice(b"x");
    assert!(decode_rasm(&build(
        &[("A", "a.ranim", 1.0, true)],
        &[(u32::MAX, 0, 0.0, 0.0, 1, unknown_condition)],
    ))
    .is_none());

    let mut bad_flags = good.clone();
    let last = bad_flags.len() - 1;
    bad_flags[last] = 0b1000_0000;
    assert!(decode_rasm(&bad_flags).is_none());
}

#[test]
fn a_machine_without_transitions_still_decodes() {
    let machine = decode_rasm(&build(&[("Only", "only.ranim", 1.0, true)], &[])).expect("decodes");
    assert_eq!(machine.states.len(), 1);
    assert!(machine.transitions.is_empty());
}
