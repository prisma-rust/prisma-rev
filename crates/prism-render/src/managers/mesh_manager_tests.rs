use super::*;

#[test]
fn build_vertices_pads_missing_attributes() {
    let input = MeshUploadInput {
        positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0]],
        normals: vec![[0.0, 1.0, 0.0]],
        colors: vec![],
        uvs: vec![[0.5, 0.5]],
        tangents: vec![],
        indices: vec![],
    };
    let v = build_vertices(&input);
    assert_eq!(v.len(), 2);
    // 缺少 normal/uv/tangent fill with safe defaults.
    assert_eq!(v[0].normal, [0.0, 1.0, 0.0]);
    assert_eq!(v[1].normal, [0.0, 1.0, 0.0]);
    assert_eq!(v[0].uv, [0.5, 0.5]);
    assert_eq!(v[1].uv, [0.0, 0.0]);
    assert_eq!(v[0].tangent, [1.0, 0.0, 0.0, 1.0]);
    assert_eq!(v[1].color, [1.0, 1.0, 1.0]);
}

#[test]
fn new_manager_is_empty() {
    let m = RenderMeshManager::new();
    assert_eq!(m.len(), 0);
    assert!(m.is_empty());
}

#[test]
fn skin_attributes_expand_into_one_entry_per_vertex() {
    let skin = SkinAttributes {
        joints: vec![[1, 2, 3, 4], [0, 0, 0, 0]],
        weights: vec![[0.25, 0.25, 0.25, 0.25], [1.0, 0.0, 0.0, 0.0]],
    };
    let vertices = skin_vertices(&skin, 2).expect("matching counts");
    assert_eq!(vertices.len(), 2);
    assert_eq!(vertices[0].joints, [1, 2, 3, 4]);
    assert_eq!(vertices[0].weights, [0.25, 0.25, 0.25, 0.25]);
    assert_eq!(vertices[1].joints, [0, 0, 0, 0]);
}

#[test]
fn mismatched_skin_attributes_are_dropped_instead_of_misread() {
    let skin = SkinAttributes {
        joints: vec![[0, 0, 0, 0]],
        weights: vec![[1.0, 0.0, 0.0, 0.0]],
    };
    // 3 vertices, 1 skin entry: dropping the skin is the only safe option.
    assert!(skin_vertices(&skin, 3).is_none());
    // Empty skin is empty, not "all the same bone".
    assert!(SkinAttributes::default().is_empty());
}

#[test]
fn the_skin_binding_is_the_second_vertex_buffer() {
    let binding = crate::mesh::SkinVertex::binding_description();
    assert_eq!(binding.binding, 1);
    assert_eq!(binding.stride, 24, "4 x u16 joints + 4 x f32 weights");
    let attrs = crate::mesh::SkinVertex::attribute_descriptions();
    assert_eq!(attrs[0].location, 5);
    assert_eq!(attrs[0].offset, 0);
    assert_eq!(attrs[1].location, 6);
    assert_eq!(attrs[1].offset, 8);
    assert_eq!(attrs[0].binding, 1);
    assert_eq!(attrs[1].binding, 1);
}
