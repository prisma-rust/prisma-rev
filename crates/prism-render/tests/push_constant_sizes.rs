//! The bytes we hand to `vkCmdPushConstants` must be exactly the reflected
//! block: the generated structs are sent through `bytemuck::bytes_of`, which
//! yields `size_of`, so the generator emits padding until the two agree.
//!
//! A shader edit that grows the block without growing the struct (or the other
//! way around) would make the GPU read past the pushed bytes, or make us send
//! fewer bytes than the shader declares. Both are silent at runtime, so the
//! invariant is pinned here instead.

use prism_render::shader_bindings;

macro_rules! assert_push_size {
    ($module:ident, $ty:ident) => {
        assert_eq!(
            std::mem::size_of::<shader_bindings::$module::$ty>(),
            shader_bindings::$module::PUSH_CONSTANT_SIZE as usize,
            "{} does not match its reflected push-constant size",
            stringify!($ty)
        );
    };
}

#[test]
fn generated_push_structs_match_their_reflected_size() {
    assert_push_size!(gizmo, GizmoPush);
    assert_push_size!(gtao, GtaoPush);
    assert_push_size!(mesh_vert, MeshPush);
    assert_push_size!(particles, ParticlePush);
    assert_push_size!(post, PostPush);
    assert_push_size!(pt_render, PtPush);
    assert_push_size!(rt_preview, RtPreviewPush);
    assert_push_size!(rt_render, RtRenderPush);
    assert_push_size!(scene_frag, PbrBindlessPush);
    assert_push_size!(shadow_depth, ShadowPush);
    assert_push_size!(skybox, SkyboxPush);
    assert_push_size!(sprites, SpritePush);
}
