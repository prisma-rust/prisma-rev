// 2D sprite pass — instanced quads sampled through the bindless texture table.
//
// Drawn inside ForwardPass' HDR render pass (after the 3D mesh draws, before
// the debug gizmo), so sprites land in the same linear color target that
// PostPass tonemaps. `tonemap_mode = 2` (passthrough) keeps the pixel-art
// palette intact.
//
// Sets (must match `SpritePass::ensure_pipeline`):
//   set 0, binding 0 - bindless SamplerState[] (globalSamplers, shared table)
//   set 0, binding 1 - bindless Texture2D[] (bindlessSrvs, shared table)
//   set 1, binding 0 - StructuredBuffer<SpriteLight> (2D lights, #611)
// The bindless layout is bound at set 0 here (it is bound at set 1 in the
// scene pipeline; the layout object is index-agnostic).
//
// Push constants: SpritePush (80 bytes) = world-view-projection + light params
// (`lightParams.x` = number of valid entries in the light buffer).
//
// Per-instance vertex data (one 112-byte record, instance rate):
//   loc 0..3 : the four world-space quad corners, BL, BR, TR, TL (float3 each)
//   loc 4    : uv rect (u0, v0, u1, v1)
//   loc 5    : color (linear RGBA, multiplied with the sampled texel)
//   loc 6    : bindless texture index
//   loc 7    : world-space clip rect (min_x, min_y, max_x, max_y); max < min = off
//   loc 8    : flags (bit 0 = receive 2D lights, #611)
//
// Sprites expand their model matrix on the CPU; 2D lines hand over explicit
// world-space triangles (a triangle is a quad whose last two corners coincide).
//
// Compile:
//   slangc sprites.slang -profile spirv_1_4 -target spirv \
//          -entry vertexMain -stage vertex -fvk-use-entrypoint-name \
//          -o sprites.vert.spv
//   slangc sprites.slang -profile spirv_1_4 -target spirv \
//          -entry fragmentMain -stage fragment -fvk-use-entrypoint-name \
//          -o sprites.frag.spv

[[vk::binding(0, 0)]]
SamplerState globalSamplers[];

[[vk::binding(1, 0)]]
Texture2D bindlessSrvs[];

// Sampler slot order is fixed by `bindless::SamplerType` (see
// `RenderTextureManager`): 0 = linear wrap, 1 = linear clamp,
// 2 = nearest, 3 = shadow.
static const uint SAMPLER_NEAREST = 2u;

// Mirrors `prism_render::SpriteLight` (three float4 = 48 bytes). The engine
// keeps 64-byte records so `std430` array stride stays 16-byte aligned.
struct SpriteLight
{
    // xy = world position, z = falloff radius (0 = none), w = intensity
    float4 positionRange;
    // rgb = linear colour, w = kind (0 = ambient, 1 = point, 2 = directional)
    float4 colorKind;
    // xy = direction (world space); zw reserved
    float4 direction;
};

[[vk::binding(0, 1)]]
StructuredBuffer<SpriteLight> spriteLights;

struct SpritePush
{
    float4x4 viewProj;
    // x = light count; yzw reserved
    float4 lightParams;
};
[[vk::push_constant]]
ConstantBuffer<SpritePush> pc;

static const uint LIGHT_KIND_AMBIENT = 0u;
static const uint LIGHT_KIND_POINT = 1u;

static const uint SPRITE_FLAG_RECEIVE_LIGHTS = 1u;

struct SpriteVertexIn
{
    [[vk::location(0)]] float3 corner0; // bottom-left
    [[vk::location(1)]] float3 corner1; // bottom-right
    [[vk::location(2)]] float3 corner2; // top-right
    [[vk::location(3)]] float3 corner3; // top-left
    [[vk::location(4)]] float4 uvRect;
    [[vk::location(5)]] float4 color;
    [[vk::location(6)]] uint   textureIndex;
    [[vk::location(7)]] float4 clip;
    [[vk::location(8)]] uint   flags;
};

struct SpriteVaryings
{
    float4 clipPos : SV_Position;
    float2 uv      : TEXCOORD0;
    float4 color   : COLOR0;
    uint   textureIndex : TEXCOORD1;
    float3 worldPos : TEXCOORD2;
    nointerpolation float4 clip : TEXCOORD3;
    nointerpolation uint flags : TEXCOORD4;
};

// Unit quad as two triangles. `QUAD_CORNER` indexes the four corner records;
// `QUAD_UV` is the matching (u, v) fraction of the sprite rect (texture rows
// arrive top-to-bottom, so the quad's top edge maps to `v0`).
static const uint QUAD_CORNER[6] = { 0u, 1u, 2u, 0u, 2u, 3u };
static const float2 QUAD_UV[6] = {
    float2(0.0, 1.0), float2(1.0, 1.0), float2(1.0, 0.0),
    float2(0.0, 1.0), float2(1.0, 0.0), float2(0.0, 0.0),
};

[shader("vertex")]
SpriteVaryings vertexMain(SpriteVertexIn in, uint vertexId : SV_VertexID)
{
    float3 corners[4] = { in.corner0, in.corner1, in.corner2, in.corner3 };
    float3 world = corners[QUAD_CORNER[vertexId]];
    float4 worldPos = float4(world, 1.0);

    float2 uv = lerp(in.uvRect.xy, in.uvRect.zw, QUAD_UV[vertexId]);

    SpriteVaryings out;
    out.clipPos = mul(pc.viewProj, worldPos);
    out.uv = uv;
    out.color = in.color;
    out.textureIndex = in.textureIndex;
    out.worldPos = world;
    out.clip = in.clip;
    out.flags = in.flags;
    return out;
}

[shader("fragment")]
float4 fragmentMain(SpriteVaryings in) : SV_Target0
{
    // Per-fragment world-space clipping (#582). `max < min` = clip disabled, so
    // a plain sprite still costs one compare on a constant value. Discarding
    // here keeps the whole 2D layer in a single draw call — no per-instance
    // scissor or extra pass.
    if (in.clip.z >= in.clip.x
        && (in.worldPos.x < in.clip.x || in.worldPos.x > in.clip.z
            || in.worldPos.y < in.clip.y || in.worldPos.y > in.clip.w))
        discard;

    // `0xFFFFFFFF` = solid fill (LineRenderer quads): no texture fetch.
    if (in.textureIndex == 0xFFFFFFFFu)
        return in.color;

    // Nearest sampling keeps pixel art crisp; the rect is pre-clamped on the
    // CPU so the sprite sheet frame never bleeds into its neighbours.
    float4 texel = bindlessSrvs[NonUniformResourceIndex(in.textureIndex)]
        .SampleLevel(globalSamplers[NonUniformResourceIndex(SAMPLER_NEAREST)], in.uv, 0.0);
    float4 color = texel * in.color;
    // Straight alpha: the pipeline blends SRC_ALPHA / ONE_MINUS_SRC_ALPHA.
    return color;
}
