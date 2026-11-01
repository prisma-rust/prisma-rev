// UI overlay — screen-space colored quads + textured glyph quads drawn on
// top of the final scene.
//
// Two quad kinds share one pipeline:
//   - plain colored fills (backgrounds): uv = (0,0) → samples the atlas
//     reserve pixel, which is white, so alpha = 1 and the vertex color
//     passes through;
//   - glyph quads (text): uv spans the glyph's rect in the R8 glyph atlas
//     (fontdue-rasterized); the sampled alpha modulates the vertex color.
//
// Sets (must match `UiOverlay::create_pipeline`):
//   set 0, binding 0 - font glyph atlas (R8 UNORM, combined image sampler)
//
// Compile:
//   slangc ui_overlay.slang -profile spirv_1_5 -target spirv \
//          -entry vertexMain -stage vertex   -fvk-use-entrypoint-name -o ui_overlay.vert.spv
//   slangc ui_overlay.slang -profile spirv_1_5 -target spirv \
//          -entry fragmentMain -stage fragment -fvk-use-entrypoint-name -o ui_overlay.frag.spv

// ---- set 0, binding 0 - glyph atlas (combined image+sampler). Pixel (0,0)
// is reserved as white so untextured quads (uv = 0) sample 1.0.
[[vk::binding(0, 0)]]
Sampler2D glyphAtlas;

struct UiVertexIn
{
    float2 position : POSITION;   // location 0 — NDC position
    float2 uv       : TEXCOORD0;  // location 1 — atlas uv (0,0 for plain quads)
    float4 color    : COLOR0;     // location 2 — RGBA color
    float2 local    : TEXCOORD1;  // location 3 — pixel offset from quad centre
    float2 halfSize : TEXCOORD2;  // location 4 — quad half size in pixels
    float  radius   : TEXCOORD3;  // location 5 — corner radius in pixels (<0 = skip)
};

struct UiVaryings
{
    float4 clipPos  : SV_Position;
    float2 uv       : TEXCOORD0;
    float4 color    : COLOR0;
    float2 local    : TEXCOORD1;
    float2 halfSize : TEXCOORD2;
    float  radius   : TEXCOORD3;
};

[shader("vertex")]
UiVaryings vertexMain(UiVertexIn in)
{
    UiVaryings out;
    out.clipPos  = float4(in.position, 0.0, 1.0);
    out.uv       = in.uv;
    out.color    = in.color;
    out.local    = in.local;
    out.halfSize = in.halfSize;
    out.radius   = in.radius;
    return out;
}

// Signed distance to a rounded box centred at the origin (pixel space).
float sdRoundBox(float2 p, float2 b, float r)
{
    float2 q = abs(p) - b + r;
    return min(max(q.x, q.y), 0.0) + length(max(q, 0.0)) - r;
}

[shader("fragment")]
float4 fragmentMain(UiVaryings in) : SV_Target0
{
    // Glyph coverage modulates the vertex color (plain quads sample the white
    // reserve pixel → coverage = 1).
    float coverage = glyphAtlas.Sample(in.uv).r;

    // Rounded rectangle (#634): glyph quads pass radius < 0 and keep the
    // plain glyph-atlas coverage. Plain quads get an SDF edge in pixel space,
    // antialiased by one pixel.
    if (in.radius >= 0.0)
    {
        float d = sdRoundBox(in.local, in.halfSize, in.radius);
        coverage *= 1.0 - smoothstep(-1.0, 1.0, d);
    }

    // Straight alpha: the pipeline blends with SRC_ALPHA/ONE_MINUS_SRC_ALPHA,
    // so coverage belongs on alpha only. Multiplying rgb here as well would
    // apply it twice — the classic dark fringe around antialiased text.
    return float4(in.color.rgb, in.color.a * coverage);
}
