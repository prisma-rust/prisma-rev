//! Best-of-N kernel benchmark on top of the reusable GPU harness.
//!
//! The shader is a grid-stride kernel: every invocation walks indices with a
//! stride equal to `groups * workgroup_size`, so all candidate group counts
//! produce the same output. The test validates every candidate, times several
//! dispatches per candidate, and selects the fastest one.
//!
//! Absolute timings are machine-dependent, so the assertions only require
//! correctness and a non-zero measurement. The selected candidate is printed
//! to make regressions visible in CI logs.

mod support;

use std::time::Duration;

use support::{BufferSpec, GpuHarness};

const COUNT: usize = 8192;
const REPEAT: u32 = 16;
const WORKGROUP_SIZE: u32 = 64;
const CANDIDATES: [u32; 4] = [16, 32, 64, 128];
const TIMED_REPEATS: u32 = 3;

fn encode_f32(values: &[f32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

fn decode_f32(bytes: &[u8], index: usize) -> f32 {
    let start = index * 4;
    f32::from_le_bytes(bytes[start..start + 4].try_into().unwrap())
}

fn reference(value: f32) -> f32 {
    let mut value = value;
    for _ in 0..REPEAT {
        // Match the two separate operations in harness_bench.slang so the
        // reference does not depend on FMA contraction.
        value *= 1.5;
        value += 0.25;
    }
    value
}

#[test]
fn best_of_n_picks_the_fastest_correct_group_count() {
    let Some(harness) = GpuHarness::new() else {
        return;
    };

    let input: Vec<f32> = (0..COUNT).map(|index| index as f32 * 0.001).collect();
    let input_bytes = encode_f32(&input);
    let expected: Vec<f32> = input.iter().map(|&value| reference(value)).collect();

    let mut results = Vec::with_capacity(CANDIDATES.len());
    for groups in CANDIDATES {
        let stride = groups * WORKGROUP_SIZE;
        let push: Vec<u8> = [COUNT as u32, REPEAT, stride, 0]
            .into_iter()
            .flat_map(u32::to_le_bytes)
            .collect();

        let timed = harness
            .dispatch_repeated(
                include_bytes!("../../../assets/shaders/harness_bench.comp.spv"),
                c"main",
                &[
                    BufferSpec::input(&input_bytes),
                    BufferSpec::output(COUNT * 4),
                ],
                &push,
                [groups, 1, 1],
                TIMED_REPEATS,
            )
            .expect("benchmark dispatch failed");

        let output = &timed.outputs[1];
        assert_eq!(output.len(), COUNT * 4);
        for (index, &expected) in expected.iter().enumerate() {
            let actual = decode_f32(output, index);
            assert!(
                (actual - expected).abs() <= 1e-5,
                "groups={groups}, index={index}: expected {expected}, got {actual}"
            );
        }

        let per_dispatch = timed.elapsed / TIMED_REPEATS;
        eprintln!(
            "kernel benchmark: groups={groups:3} total={:?} per-dispatch={per_dispatch:?}",
            timed.elapsed
        );
        results.push((groups, per_dispatch));
    }

    let (best_groups, best_time) = results
        .iter()
        .min_by_key(|(_, elapsed)| *elapsed)
        .copied()
        .expect("at least one candidate");
    assert!(best_time > Duration::ZERO, "timing must be non-zero");
    eprintln!("kernel benchmark: best groups={best_groups} ({best_time:?}/dispatch)");
}
