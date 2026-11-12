use super::*;
#[test]
fn push_constant_size() {
    // The auto-generated PtPush is 176 bytes (repr(C)) after the SHARC
    // fields were appended; the std140 block reflected from the shader and
    // the VkPushConstantRange must agree, otherwise the tail fields arrive
    // as garbage.
    assert_eq!(
        std::mem::size_of::<shader_bindings::pt_render::PtPush>(),
        176,
        "shader_bindings::pt_render::PtPush (repr(C))"
    );
    assert_eq!(
        PT_PUSH_RANGE_SIZE as usize,
        std::mem::size_of::<shader_bindings::pt_render::PtPush>(),
        "PT_PUSH_RANGE_SIZE must match the reflected push block"
    );
    assert_eq!(
        PT_PUSH_RANGE_SIZE,
        shader_bindings::pt_render::PUSH_CONSTANT_SIZE,
        "PT_PUSH_RANGE_SIZE must match the shader's reflected block size"
    );
}

/// `params.w` bit layout must match the `PT_PARAMS_*` constants in
/// `pt_render.slang`: bits 0..=29 frame counter, bit 30 orthographic camera,
/// bit 31 reset accumulation.
#[test]
fn pt_params_packing_matches_shader_bits() {
    assert_eq!(PT_PARAMS_FRAME_MASK, 0x3FFF_FFFF);
    assert_eq!(PT_PARAMS_ORTHOGRAPHIC, 1 << 30);
    assert_eq!(PT_PARAMS_RESET, 1 << 31);

    assert_eq!(pack_pt_params(0, false, false), 0);
    assert_eq!(pack_pt_params(42, false, false), 42);
    assert_eq!(pack_pt_params(42, true, false), 42 | (1 << 31));
    assert_eq!(pack_pt_params(42, false, true), 42 | (1 << 30));
    assert_eq!(pack_pt_params(42, true, true), 42 | (1 << 30) | (1 << 31));

    // The frame counter must not overflow into the flag bits — the shader
    // unpacks with the same mask.
    assert_eq!(pack_pt_params(u32::MAX, false, false), PT_PARAMS_FRAME_MASK);
}
