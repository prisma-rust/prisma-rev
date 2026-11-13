use super::*;

fn default_input() -> MaterialUploadInput {
    MaterialUploadInput {
        base_color: [1.0, 0.5, 0.2, 1.0],
        metallic: 0.8,
        roughness: 0.3,
        emissive: [0.0, 0.0, 0.0],
        albedo_tex: None,
        normal_tex: None,
        metallic_roughness_tex: None,
        emissive_tex: None,
        occlusion_tex: None,
        normal_scale: 1.0,
        occlusion_strength: 1.0,
        transmission: 0.0,
        ior: 1.5,
        translucency: 0.0,
        anisotropy: 0.0,
        clearcoat: 0.0,
        clearcoat_roughness: 0.0,
        emissive_strength: 1.0,
        alpha_mode: MaterialAlphaMode::Mask,
        alpha_cutoff: DEFAULT_ALPHA_CUTOFF,
    }
}

#[test]
fn gpu_material_layout_is_112_bytes() {
    assert_eq!(std::mem::size_of::<GpuMaterial>(), 112);
    assert_eq!(std::mem::align_of::<GpuMaterial>(), 16);
}

#[test]
fn gpu_material_offsets() {
    let m = GpuMaterial {
        base_color: [0.0; 4],
        metallic_roughness_emissive: [0.0; 4],
        albedo_idx: 0,
        normal_idx: 0,
        metallic_roughness_idx: 0,
        emissive_idx: 0,
        transmission_factor: [0.0; 4],
        clearcoat: [0.0; 4],
        transmission_tex_idx: 0,
        occlusion_idx: 0,
        normal_scale: 0.0,
        occlusion_strength: 0.0,
        alpha_cutoff: 0.0,
        alpha_mode: 1,
        alpha_pad: [0.0; 2],
    };
    let base_ptr = &m as *const _ as usize;
    assert_eq!((&m.base_color as *const _ as usize) - base_ptr, 0);
    assert_eq!(
        (&m.metallic_roughness_emissive as *const _ as usize) - base_ptr,
        16
    );
    assert_eq!((&m.albedo_idx as *const _ as usize) - base_ptr, 32);
    assert_eq!((&m.normal_idx as *const _ as usize) - base_ptr, 36);
    assert_eq!(
        (&m.metallic_roughness_idx as *const _ as usize) - base_ptr,
        40
    );
    assert_eq!((&m.emissive_idx as *const _ as usize) - base_ptr, 44);
    assert_eq!((&m.transmission_factor as *const _ as usize) - base_ptr, 48);
    assert_eq!((&m.clearcoat as *const _ as usize) - base_ptr, 64);
    assert_eq!(
        (&m.transmission_tex_idx as *const _ as usize) - base_ptr,
        80
    );
    assert_eq!((&m.occlusion_idx as *const _ as usize) - base_ptr, 84);
    assert_eq!((&m.normal_scale as *const _ as usize) - base_ptr, 88);
    assert_eq!((&m.occlusion_strength as *const _ as usize) - base_ptr, 92);
    assert_eq!((&m.alpha_cutoff as *const _ as usize) - base_ptr, 96);
    assert_eq!((&m.alpha_mode as *const _ as usize) - base_ptr, 100);
    assert_eq!((&m.alpha_pad as *const _ as usize) - base_ptr, 104);
}

#[test]
fn to_gpu_carries_alpha_semantics() {
    let mut input = default_input();
    input.alpha_mode = MaterialAlphaMode::Opaque;
    input.alpha_cutoff = 0.25;
    let gpu = input.to_gpu();
    assert_eq!(gpu.alpha_mode, 0, "opaque encodes as 0");
    assert_eq!(gpu.alpha_cutoff, 0.25);
    assert!(input.is_opaque());

    input.alpha_mode = MaterialAlphaMode::Mask;
    assert_eq!(input.to_gpu().alpha_mode, 1);
    assert!(!input.is_opaque());
}

#[test]
fn to_gpu_packs_textures_as_invalid_when_none() {
    let input = default_input();
    let gpu = input.to_gpu();
    assert_eq!(gpu.base_color, [1.0, 0.5, 0.2, 1.0]);
    assert_eq!(gpu.metallic_roughness_emissive[0], 0.8);
    assert_eq!(gpu.metallic_roughness_emissive[1], 0.3);
    assert_eq!(gpu.metallic_roughness_emissive[3], 1.0); // emissive_strength
    assert_eq!(gpu.albedo_idx, u32::MAX);
    assert_eq!(gpu.normal_idx, u32::MAX);
    assert_eq!(gpu.metallic_roughness_idx, u32::MAX);
    assert_eq!(gpu.emissive_idx, u32::MAX);
    // Advanced fields
    assert_eq!(gpu.transmission_factor[0], 0.0);
    assert_eq!(gpu.transmission_factor[1], 1.5);
    assert_eq!(gpu.transmission_factor[2], 0.0);
    assert_eq!(gpu.transmission_factor[3], 0.0);
    assert_eq!(gpu.clearcoat[0], 0.0);
    assert_eq!(gpu.clearcoat[1], 0.0);
    assert_eq!(gpu.transmission_tex_idx, u32::MAX);
    assert_eq!(gpu.occlusion_idx, u32::MAX);
    assert_eq!(gpu.normal_scale, 1.0);
}

#[test]
fn to_gpu_packs_textures_when_present() {
    let input = MaterialUploadInput {
        albedo_tex: Some(7),
        normal_tex: Some(11),
        metallic_roughness_tex: Some(13),
        emissive_tex: Some(17),
        occlusion_tex: Some(19),
        normal_scale: 0.75,
        ..default_input()
    };
    let gpu = input.to_gpu();
    assert_eq!(gpu.albedo_idx, 7);
    assert_eq!(gpu.normal_idx, 11);
    assert_eq!(gpu.metallic_roughness_idx, 13);
    assert_eq!(gpu.emissive_idx, 17);
    assert_eq!(gpu.occlusion_idx, 19);
    assert_eq!(gpu.normal_scale, 0.75);
}

#[test]
fn to_gpu_packs_advanced_fields() {
    let input = MaterialUploadInput {
        transmission: 0.5,
        ior: 1.45,
        translucency: 0.3,
        anisotropy: 0.6,
        clearcoat: 0.2,
        clearcoat_roughness: 0.1,
        emissive_strength: 2.5,
        ..default_input()
    };
    let gpu = input.to_gpu();
    assert_eq!(gpu.transmission_factor[0], 0.5);
    assert_eq!(gpu.transmission_factor[1], 1.45);
    assert_eq!(gpu.transmission_factor[2], 0.3);
    assert_eq!(gpu.transmission_factor[3], 0.6);
    assert_eq!(gpu.clearcoat[0], 0.2);
    assert_eq!(gpu.clearcoat[1], 0.1);
    assert_eq!(gpu.metallic_roughness_emissive[3], 2.5);
}
