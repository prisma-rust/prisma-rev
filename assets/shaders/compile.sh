#!/usr/bin/env bash
# Compile PrismaRev Slang shaders to SPIR-V + emit reflection JSON.
#
# This runs on a DESKTOP / CI host (Windows/Linux/macOS x86_64), NOT on the
# Termux/aarch64 device — the official slangc prebuilts are glibc/MSVC binaries.
# On Android the engine ships the pre-compiled .spv produced here.
#
# Output per stage:
#   <name>.<stage>.spv          — SPIR-V for the Vulkan pipeline
#   reflection/<name>.json      — slang reflection (drives Rust binding codegen)
#
# Requires `slangc` on PATH (from a Slang release), or set SLANGC=/path/to/slangc.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SRC="$HERE/slang"
OUT="$HERE"                      # .spv land next to the existing GLSL .spv
REFL="$HERE/reflection"
SLANGC="${SLANGC:-slangc}"
PROFILE="${SLANG_PROFILE:-spirv_1_4}"

if ! command -v "$SLANGC" >/dev/null 2>&1 && [ ! -x "$SLANGC" ]; then
  echo "ERROR: slangc not found. Install a Slang release or set SLANGC=/path/to/slangc" >&2
  echo "  e.g. tools/slang/bin/slangc (desktop only — glibc binary won't run under Termux)" >&2
  exit 1
fi

mkdir -p "$REFL"

# name : entry:stage pairs (space separated). stage = vert|frag (matches the
# .spv filenames the engine include_bytes!'s, e.g.
# mesh_vert.vert.spv / scene_frag.frag.spv / shadow_depth.vert.spv).
# Slang entry points are vertexMain / fragmentMain (see the [shader(...)] attrs).
compile_stage() {
  local name="$1" entry="$2" stage="$3"
  # Map slang stage names (vertex/fragment) to the file extension the engine
  # expects (vert/frag), so the generated .spv matches include_bytes! paths.
  case "$stage" in
    vertex)   ext=vert ;;
    fragment) ext=frag ;;
    *)        ext="$stage" ;;
  esac
  local out_spv="$OUT/${name}.${ext}.spv"
  echo "  $name :: $entry -> ${name}.${ext}.spv"
  "$SLANGC" "$SRC/${name}.slang" \
    -profile "$PROFILE" \
    -target spirv \
    -entry "$entry" \
    -stage "$stage" \
    -fvk-use-entrypoint-name \
    -o "$out_spv"
}

emit_reflection() {
  # One reflection JSON per module (covers all entry points + bindings).
  local name="$1"; shift
  local entries=()
  while [ "$#" -gt 0 ]; do entries+=(-entry "$1" -stage "$2"); shift 2; done
  echo "  reflect $name -> reflection/${name}.json"
  # slangc requires an SPIR-V output path even for reflection-only runs.
  # Write to a throwaway file and delete it immediately so we don't leave
  # .tmp.spv litter in reflection/ (which would pollute git status).
  # (We can't use /dev/null: on Windows bash that maps to a literal "nul"
  # path slangc can't open.)
  local tmp="$REFL/${name}.tmp.spv"
  "$SLANGC" "$SRC/${name}.slang" \
    -profile "$PROFILE" \
    -target spirv \
    "${entries[@]}" \
    -reflection-json "$REFL/${name}.json" \
    -o "$tmp"
  rm -f "$tmp"
}

# fix_spv <file.spv>
# Slang emits an illegal `ArrayStride` decoration on UniformConstant runtime
# arrays of opaque types (sampled images / samplers) used for bindless
# descriptor indexing. SPIR-V validation rejects explicit layout decorations on
# such runtime arrays (VUID-StandaloneSpirv-None-10684), so the module fails to
# load under the validation layer. fix_spirv.py strips only those illegal
# decorations. Safe to run on any .spv; it leaves legal strides untouched.
fix_spv() {
  local spv="$1"
  if command -v python3 >/dev/null 2>&1; then
    python3 "$HERE/fix_spirv.py" "$spv" "$spv"
  else
    echo "  WARNING: python3 not found; skipping ArrayStride fix for $spv" >&2
  fi
}

echo "Compiling Slang shaders (slangc = $SLANGC, profile = $PROFILE)..."

# mesh_vert: vertex stage for the ForwardPass (shared by all scene geometry).
compile_stage mesh_vert vertexMain vertex
emit_reflection mesh_vert vertexMain vertex

# gizmo: vertex + fragment
compile_stage gizmo vertexMain vertex
compile_stage gizmo fragmentMain fragment
emit_reflection gizmo vertexMain vertex fragmentMain fragment

# shadow_depth: vertex + fragment (rasterized depth-only shadow map).
compile_stage shadow_depth vertexMain vertex
compile_stage shadow_depth fragmentMain fragment
emit_reflection shadow_depth vertexMain vertex fragmentMain fragment

# skybox: vertex + fragment (environment cubemap background).
# Single source file with two entry points (vertexMain / fragmentMain).
echo "  skybox :: vertexMain -> skybox.vert.spv"
"$SLANGC" "$SRC/skybox.slang" \
  -profile "$PROFILE" -target spirv -entry vertexMain -stage vertex \
  -fvk-use-entrypoint-name -o "$OUT/skybox.vert.spv"
echo "  skybox :: fragmentMain -> skybox.frag.spv"
"$SLANGC" "$SRC/skybox.slang" \
  -profile "$PROFILE" -target spirv -entry fragmentMain -stage fragment \
  -fvk-use-entrypoint-name -o "$OUT/skybox.frag.spv"
echo "  reflect skybox -> reflection/skybox.json"
"$SLANGC" "$SRC/skybox.slang" \
  -profile "$PROFILE" -target spirv \
  -entry vertexMain -stage vertex -entry fragmentMain -stage fragment \
  -reflection-json "$REFL/skybox.json" \
  -o "$REFL/skybox.tmp.spv"
rm -f "$REFL/skybox.tmp.spv"

# scene_frag: fragment only (RenderGraph ForwardPass bindless PBR + rasterized
# shadow map). Pairs with mesh_vert.vert.spv. The light-space view-projection
# for the shadow map is read from the per-frame UBO (not push constants) so
# the push constant stays under the 128-byte Vulkan limit.
compile_stage scene_frag fragmentMain fragment
fix_spv "$OUT/scene_frag.frag.spv"
emit_reflection scene_frag fragmentMain fragment

# post: vertex + fragment (fullscreen HDR -> sRGB swapchain tonemap). Self
# contained (no bindless/IBL), so no ArrayStride fix-up needed.
compile_stage post vertexMain vertex
compile_stage post fragmentMain fragment
emit_reflection post vertexMain vertex fragmentMain fragment

# gtao: vertex + fragment (half-res screen-space ambient occlusion). Reads the
# ForwardPass depth + view-normal MRT and writes an R8 visibility texture sampled
# by the scene shader to attenuate IBL. Self-contained (no bindless/IBL).
compile_stage gtao vertexMain vertex
compile_stage gtao fragmentMain fragment
emit_reflection gtao vertexMain vertex fragmentMain fragment

# gi_bake: compute shader for offline probe-volume GI baking (ray-query).
# Dispatched (dims.x, dims.y, dims.z) — one thread per probe. Requires
# VK_KHR_ray_query hardware. Output: RWTexture3D coefficient-major depth slices.
echo "  gi_bake :: bakeMain -> gi_bake.comp.spv"
"$SLANGC" "$SRC/gi_bake.slang" \
  -profile "$PROFILE" -target spirv -entry bakeMain -stage compute \
  -fvk-use-entrypoint-name -o "$OUT/gi_bake.comp.spv"
fix_spv "$OUT/gi_bake.comp.spv"

# gi_render: compute shader for offline path-traced image rendering (ray-query).
# Dispatched per pixel — renders an HDR image directly. Requires
# VK_KHR_ray_query hardware. Output: RWTexture2D RGBA32F.
echo "  gi_render :: renderMain -> gi_render.comp.spv"
"$SLANGC" "$SRC/gi_render.slang" \
  -profile "$PROFILE" -target spirv -entry renderMain -stage compute \
  -fvk-use-entrypoint-name -o "$OUT/gi_render.comp.spv"
fix_spv "$OUT/gi_render.comp.spv"

# pt_render: real-time path tracing compute shader (temporal accumulation).
# Dispatched per pixel — traces 1 sample/frame, accumulates for convergence.
# Requires VK_KHR_ray_query hardware. Output: RWTexture2D RGBA32F (accumulated).
echo "  pt_render :: ptMain -> pt_render.comp.spv"
"$SLANGC" "$SRC/pt_render.slang" \
  -profile "$PROFILE" -target spirv -entry ptMain -stage compute \
  -fvk-use-entrypoint-name -o "$OUT/pt_render.comp.spv"
fix_spv "$OUT/pt_render.comp.spv"
emit_reflection pt_render ptMain compute

# ui_overlay: vertex + fragment (coloured quads on top of the scene output).
# Self-contained pipeline (no bindless/IBL).
compile_stage ui_overlay vertexMain vertex
compile_stage ui_overlay fragmentMain fragment
emit_reflection ui_overlay vertexMain vertex fragmentMain fragment

# sprites: vertex + fragment (2D sprite quads sampled through the bindless
# texture table, drawn inside ForwardPass' HDR render pass).
compile_stage sprites vertexMain vertex
compile_stage sprites fragmentMain fragment
# Slang puts an illegal ArrayStride on the bindless opaque runtime arrays;
# strip it before validation/startup (#580).
fix_spv "$OUT/sprites.frag.spv"
emit_reflection sprites vertexMain vertex fragmentMain fragment

# particles: vertex + fragment (world-space billboards, no texture: the radial
# alpha falloff is computed from the quad UV). Drawn inside ForwardPass' HDR
# render pass with the depth test on, so scene geometry occludes them.
compile_stage particles vertexMain vertex
compile_stage particles fragmentMain fragment
emit_reflection particles vertexMain vertex fragmentMain fragment

# rt_render: vertex + fragment — offscreen shader that renders a 4x4 bitmap
# pattern (from a CPU-generated 16-bit value) into a RenderTexture. The shader
# is target-agnostic; RenderToTexturePass binds the RT view as the attachment.
compile_stage rt_render vertexMain vertex
compile_stage rt_render fragmentMain fragment
emit_reflection rt_render vertexMain vertex fragmentMain fragment

# rt_preview: vertex + fragment — fullscreen consumer that samples the
# RenderTexture through the bindless SRV table (经典 RT usage).
compile_stage rt_preview vertexMain vertex
compile_stage rt_preview fragmentMain fragment
fix_spv "$OUT/rt_preview.frag.spv"
emit_reflection rt_preview vertexMain vertex fragmentMain fragment

# Hard gate: every emitted module must be valid SPIR-V (#580). Slang can emit
# illegal layout decorations that only show up as a Vulkan validation error at
# startup; fail the compile instead of shipping them.
if command -v spirv-val >/dev/null 2>&1; then
    for spv in "$OUT"/*.spv; do
        spirv-val --target-env vulkan1.2 "$spv" >/dev/null
    done
    echo "spirv-val: all modules valid"
fi

echo "All Slang shaders compiled. SPIR-V in $OUT, reflection JSON in $REFL"
