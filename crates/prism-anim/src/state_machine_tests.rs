use super::*;

fn idle_run_machine() -> StateMachine {
    StateMachine::new(
        vec![
            AnimState::new("idle", "idle.ranim"),
            AnimState::new("walk", "walk.ranim"),
            AnimState::new("run", "run.ranim"),
        ],
        vec![
            AnimTransition::new(0, 1).with_conditions(vec![TransitionCondition::FloatGreater {
                name: "speed".into(),
                threshold: 0.1,
            }]),
            AnimTransition::new(0, 2).with_conditions(vec![TransitionCondition::FloatGreater {
                name: "speed".into(),
                threshold: 4.0,
            }]),
            AnimTransition::new(1, 2).with_conditions(vec![TransitionCondition::FloatGreater {
                name: "speed".into(),
                threshold: 4.0,
            }]),
            AnimTransition::new(2, 1).with_conditions(vec![TransitionCondition::FloatLess {
                name: "speed".into(),
                threshold: 4.0,
            }]),
        ],
    )
}

#[test]
fn the_first_matching_transition_in_author_order_wins() {
    let machine = idle_run_machine();
    let mut params = AnimParameters::new();

    // 速度 5 同时满足「> 0.1」和「> 4.0」，但 idle→walk 写在前面，所以先走 walk。
    params.set_float("speed", 5.0);
    assert_eq!(
        machine
            .next_transition(0, &mut params, 0.0, 1.0)
            .map(|t| t.state),
        Some(1)
    );
    // 从 walk 出发只满足 walk→run。
    assert_eq!(
        machine
            .next_transition(1, &mut params, 0.0, 1.0)
            .map(|t| t.state),
        Some(2)
    );
    // run 状态下速度还是 5：run→walk 的条件（< 4）不成立。
    assert_eq!(machine.next_transition(2, &mut params, 0.0, 1.0), None);
}

#[test]
fn a_slow_character_does_not_leave_idle() {
    let machine = idle_run_machine();
    let mut params = AnimParameters::new();
    params.set_float("speed", 0.05);
    assert_eq!(machine.next_transition(0, &mut params, 0.0, 1.0), None);
    // 参数缺失同样不转移（数据没填好不该让角色乱动）。
    assert_eq!(
        machine.next_transition(0, &mut AnimParameters::new(), 0.0, 1.0),
        None
    );
    // NaN 也不成立。
    params.set_float("speed", f32::NAN);
    assert_eq!(machine.next_transition(0, &mut params, 0.0, 1.0), None);
}

#[test]
fn a_trigger_fires_once_and_is_then_consumed() {
    let machine = StateMachine::new(
        vec![
            AnimState::new("idle", "idle.ranim"),
            AnimState::new("jump", "jump.ranim"),
        ],
        vec![AnimTransition::new(0, 1)
            .with_conditions(vec![TransitionCondition::Trigger {
                name: "jump".into(),
            }])
            .with_duration(0.15)],
    );
    let mut params = AnimParameters::new();
    assert_eq!(machine.next_transition(0, &mut params, 0.0, 1.0), None);

    params.set_trigger("jump");
    let target = machine
        .next_transition(0, &mut params, 0.0, 1.0)
        .expect("trigger fires");
    assert_eq!(target.state, 1);
    assert!((target.duration - 0.15).abs() < 1e-6);
    assert!(!params.has_trigger("jump"), "trigger 应该被消耗掉");

    // 再求值不会重复触发。
    assert_eq!(machine.next_transition(0, &mut params, 0.0, 1.0), None);
}

#[test]
fn the_minimum_state_fraction_holds_the_transition_until_the_state_has_played() {
    let machine = StateMachine::new(
        vec![
            AnimState::new("attack", "attack.ranim"),
            AnimState::new("idle", "idle.ranim"),
        ],
        vec![AnimTransition::new(0, 1)
            .with_min_state_fraction(1.0)
            .with_conditions(vec![TransitionCondition::BoolIs {
                name: "done".into(),
                expected: true,
            }])],
    );
    let mut params = AnimParameters::new();
    params.set_bool("done", true);

    // 播到 40%：条件满足但还没到最早打断点。
    assert_eq!(machine.next_transition(0, &mut params, 0.4, 1.0), None);
    // 播完：允许转移。
    assert_eq!(
        machine
            .next_transition(0, &mut params, 1.0, 1.0)
            .map(|t| t.state),
        Some(1)
    );
    // 时长未知（0 / NaN）时按「已播完」处理，否则单次剪辑会卡在最后。
    assert_eq!(
        machine
            .next_transition(0, &mut params, 0.0, 0.0)
            .map(|t| t.state),
        Some(1)
    );
    assert_eq!(
        machine
            .next_transition(0, &mut params, 0.0, f32::NAN)
            .map(|t| t.state),
        Some(1)
    );
}

#[test]
fn transitions_from_other_states_and_self_loops_are_ignored() {
    let machine = StateMachine::new(
        vec![
            AnimState::new("a", "a.ranim"),
            AnimState::new("b", "b.ranim"),
        ],
        vec![
            // 自环：永远不选。
            AnimTransition::new(0, 0),
            // 只从 b 出发：在 a 上不生效。
            AnimTransition::new(1, 0),
            // 越界目标：跳过。
            AnimTransition::new(0, 99),
        ],
    );
    assert_eq!(
        machine.next_transition(0, &mut AnimParameters::new(), 0.0, 1.0),
        None
    );
    // 越界源状态也不 panic。
    assert_eq!(
        machine.next_transition(99, &mut AnimParameters::new(), 0.0, 1.0),
        None
    );
}

#[test]
fn an_any_state_transition_applies_from_every_state() {
    let machine = StateMachine::new(
        vec![
            AnimState::new("idle", "idle.ranim"),
            AnimState::new("walk", "walk.ranim"),
            AnimState::new("dead", "dead.ranim"),
        ],
        vec![AnimTransition::from_any(2)
            .with_conditions(vec![TransitionCondition::Trigger { name: "die".into() }])],
    );
    for from in 0..2 {
        let mut params = AnimParameters::new();
        params.set_trigger("die");
        assert_eq!(
            machine
                .next_transition(from, &mut params, 0.0, 1.0)
                .map(|t| t.state),
            Some(2),
            "from {from}"
        );
    }
}

#[test]
fn states_are_looked_up_by_name_and_bad_durations_are_sanitized() {
    let machine = idle_run_machine();
    assert_eq!(machine.state_index("walk"), Some(1));
    assert_eq!(machine.state_index("nope"), None);
    assert_eq!(
        machine.state(1).map(|state| state.clip_path.as_str()),
        Some("walk.ranim")
    );

    let machine = StateMachine::new(
        vec![
            AnimState::new("a", "a.ranim"),
            AnimState::new("b", "b.ranim"),
        ],
        vec![
            AnimTransition::new(0, 1).with_duration(f32::NAN),
            AnimTransition::new(0, 1).with_duration(-3.0),
        ],
    );
    let target = machine
        .next_transition(0, &mut AnimParameters::new(), 0.0, 1.0)
        .expect("unconditional transition");
    assert_eq!(target.duration, 0.0, "坏时长按硬切处理");
}

#[test]
fn parameters_round_trip_and_clear() {
    let mut params = AnimParameters::new();
    assert!(params.is_empty());
    params.set_float("speed", 2.5);
    params.set_bool("grounded", true);
    params.set_trigger("attack");
    assert_eq!(params.float("speed"), Some(2.5));
    assert_eq!(params.bool("grounded"), Some(true));
    assert!(params.has_trigger("attack"));
    assert_eq!(params.float("missing"), None);
    assert_eq!(params.bool("missing"), None);

    assert!(params.take_trigger("attack"));
    assert!(!params.take_trigger("attack"));
    params.set_trigger("a");
    params.set_trigger("b");
    params.clear_triggers();
    assert!(!params.has_trigger("a") && !params.has_trigger("b"));
    assert!(!params.is_empty());
}
