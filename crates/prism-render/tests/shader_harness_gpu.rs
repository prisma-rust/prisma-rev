//! End-to-end smoke test for the reusable GPU shader harness.
//!
//! The kernel (`assets/shaders/slang/harness_copy.slang`) reads a storage
//! buffer, applies push constants, and writes a second storage buffer. This
//! test proves the harness can compile/load SPIR-V, create a compute pipeline
//! and descriptor set, dispatch, synchronise, and read the result back.
//!
//! It skips gracefully when no Vulkan driver is available, matching the other
//! headless tests. CI sets `PRISM_REQUIRE_VALIDATION=1`, so the same run also
//! exercises the validation layer.

mod support;

use support::{BufferSpec, GpuHarness};

fn bytes_of<T: bytemuck::Pod>(values: &[T]) -> &[u8] {
    bytemuck::cast_slice(values)
}

fn read_u32(bytes: &[u8], index: usize) -> u32 {
    let start = index * 4;
    u32::from_ne_bytes(bytes[start..start + 4].try_into().unwrap())
}

#[test]
fn harness_runs_a_compute_kernel_with_push_constants() {
    let Some(harness) = GpuHarness::new() else {
        return;
    };

    let input: Vec<u32> = (0..16).collect();
    let push: Vec<u8> = [16u32, 3, 7]
        .into_iter()
        .flat_map(u32::to_ne_bytes)
        .collect();

    let results = harness
        .dispatch(
            include_bytes!("../../../assets/shaders/harness_copy.comp.spv"),
            c"main",
            &[
                BufferSpec::input(bytes_of(&input)),
                BufferSpec::output(16 * 4),
            ],
            &push,
            [4, 1, 1],
        )
        .expect("compute dispatch failed");

    assert_eq!(results.len(), 2);
    assert!(results[0].is_empty(), "input buffer must not be read back");

    let output = &results[1];
    assert_eq!(output.len(), 16 * 4);
    for (index, value) in input.iter().enumerate() {
        assert_eq!(
            read_u32(output, index),
            value * 3 + 7,
            "unexpected output at index {index}"
        );
    }
}
