// Shared types for the PrismaRev forward pipeline.
//
// Slang module: `import common;` from other .slang files.
//
// Layout note: these structs are laid out to match the existing GLSL std140
// (FrameUBO) / std430-ish push-constant blocks byte-for-byte, so the Rust-side
// `#[repr(C)]` mirrors (FrameUBOData / PbrPushConstants) stay valid and the
// reflection-generated bindings agree with the hand-written ones.
module common;

// Per-frame uniform data. Mirrors Rust `FrameUBOData` (set 0, binding 0).
// std140: mat4(64) + vec4(16)*3 + mat4(64) + mat4(64) + uint(4) + vec2(8) +
//         float(4) + float(4) + float3(12) + mat4(64) + float(4) + uint(4) +
//         float2(8) = 344, padded to 352.
public struct FrameUBO
{
    public float4x4 viewProj;        // offset   0
    public float4   cameraPosition;  // offset  64  (xyz = camera pos, w = point light count)
    public float4   lightDirection;  // offset  80  (xyz = dir TO light, w = pre-scaled radiance = lux/(10000*PI))
    public float4   lightColor;      // offset  96  (rgb = color, w = ambient factor)
    public float4x4 view;            // offset 112  (world -> view)
    public float4x4 lightViewProj;   // offset 176  (near cascade light-space VP)
    public uint     tonemapMode;     // offset 240 (0 = Reinhard, 1 = ACES)
    public float    viewportW;       // offset 244 - scene colour viewport width, px
    public float    viewportH;       // offset 248 - scene colour viewport height, px
    public float    exposure;        // offset 252 (multiplies all light radiance pre-tonemap)
    public float    shadowStrength;      // offset 256 (Light.shadowStrength; 0 = no shadow)
    public float3   _pad2;           // offset 260..271 (std140 tail pad)
    // #526 CSM: the second cascade lives in the right half of the atlas.
    public float4x4 lightViewProj1;  // offset 272 (far cascade light-space VP)
    public float    shadowSplit;     // offset 336 (camera-forward distance of the split)
    public uint     shadowCascadeCount; // offset 340 (1 = single map, 2 = atlas)
    public float2   _pad3;           // offset 344..351 (std140 tail pad to 352)
};

// Vertex stream matching mesh.vert input locations 0..4.
public struct MeshVertexIn
{
    public float3 position  : POSITION;   // location 0
    public float3 normal    : NORMAL;     // location 1
    public float3 color     : COLOR0;     // location 2
    public float2 uv        : TEXCOORD0;  // location 3
    public float4 tangent   : TANGENT;    // location 4 (xyz = dir, w = handedness)
};

// Interpolants shared by mesh.frag (Blinn-Phong) and pbr.frag (PBR).
// Output/input locations MUST stay in sync across the vertex + both fragment
// stages: 0 color, 1 world normal, 2 world pos, 3 uv, 4 tangent.
public struct MeshVaryings
{
    public float4 clipPos      : SV_Position;
    public float3 color        : COLOR0;     // location 0
    public float3 worldNormal  : NORMAL;     // location 1
    public float3 worldPos     : TEXCOORD1;  // location 2
    public float2 uv           : TEXCOORD0;  // location 3
    public float4 tangent      : TANGENT;    // location 4 (xyz = dir, w = handedness)
    // Skin-weight heatmap colour (#560). Only meaningful when the debug flag is
    // set; the vertex stage computes it from the dominant joint so the fragment
    // stage can emit it unlit. Zero for static geometry.
    public float3 skinHeat     : COLOR1;     // location 5
};
