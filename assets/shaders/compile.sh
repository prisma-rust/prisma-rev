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
# Reflection JSON is committed. The script never overwrites it unless asked:
# the freshly generated files go to a staging directory and are compared with
# the committed ones. `--write-reflection` adopts them (then rerun
# `xtask shader-bindgen` and commit both).
#
# Requires `slangc` on PATH (from a Slang release), or set SLANGC=/path/to/slangc.
#
#   bash assets/shaders/compile.sh                     # build + verify reflection
#   bash assets/shaders/compile.sh --write-reflection  # adopt the new reflection
#
# Env:
#   SLANGC              slangc binary (default: slangc on PATH)
#   SLANG_PROFILE       SPIR-V profile (default: spirv_1_4)
#   SLANGC_MIN_VERSION  minimum accepted slangc release (default: 2026.21;
#                       `0` disables the check — only for throwaway experiments)
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SRC="$HERE/slang"
OUT="$HERE"                      # .spv land next to the existing GLSL .spv
REFL="$HERE/reflection"
SLANGC="${SLANGC:-slangc}"
PROFILE="${SLANG_PROFILE:-spirv_1_4}"
# Release that produced the committed reflection JSON. Older compilers emit the
# same field offsets but drop schema fields (`version`, `combined`, `sizes`),
# so the generated bindings can differ from what the shader actually declares.
MIN_SLANGC="${SLANGC_MIN_VERSION:-2026.21}"
SLANG_DOWNLOAD="https://github.com/shader-slang/slang/releases"
WRITE_REFLECTION=0

usage() {
  sed -n '2,/^set -euo pipefail$/p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

for arg in "$@"; do
  case "$arg" in
    --write-reflection) WRITE_REFLECTION=1 ;;
    -h|--help) usage; exit 0 ;;
    *)
      echo "ERROR: unknown argument '$arg' (try --help)" >&2
      exit 2
      ;;
  esac
done

if ! command -v "$SLANGC" >/dev/null 2>&1 && [ ! -x "$SLANGC" ]; then
  echo "ERROR: slangc not found. Install a Slang release or set SLANGC=/path/to/slangc" >&2
  echo "  $SLANG_DOWNLOAD" >&2
  echo "  e.g. tools/slang/bin/slangc (desktop only — glibc binary won't run under Termux)" >&2
  exit 1
fi

# --------------------------------------------------------------------------- #
# slangc version
# --------------------------------------------------------------------------- #

slangc_version() {
  local raw
  # slangc writes the version to stderr and without a trailing newline.
  raw="$("$SLANGC" -version 2>&1 | head -n 1)"
  printf '%s' "${raw:-unknown}"
}

is_number() {
  case "$1" in
    ''|*[!0-9]*) return 1 ;;
    *) return 0 ;;
  esac
}

# split_version <2026.21|2025.11-12-gabc> -> "major minor patch"
split_version() {
  local major=0 minor=0 patch=0
  IFS=. read -r major minor patch <<<"${1%%-*}"
  printf '%s %s %s\n' "${major:-0}" "${minor:-0}" "${patch:-0}"
}

# version_ge <have> <want>
version_ge() {
  local a1 a2 a3 b1 b2 b3
  read -r a1 a2 a3 <<<"$(split_version "$1")"
  read -r b1 b2 b3 <<<"$(split_version "$2")"
  for n in "$a1" "$a2" "$a3" "$b1" "$b2" "$b3"; do
    is_number "$n" || return 1
  done
  if ((a1 != b1)); then ((a1 > b1)) && return 0 || return 1; fi
  if ((a2 != b2)); then ((a2 > b2)) && return 0 || return 1; fi
  ((a3 >= b3)) && return 0 || return 1
}

SLANGC_VER="$(slangc_version)"
if [ "$MIN_SLANGC" != "0" ]; then
  if ! is_number "${SLANGC_VER%%.*}"; then
    {
      echo "ERROR: could not parse '$SLANGC -version' output: '$SLANGC_VER'"
      echo "Set SLANGC_MIN_VERSION=0 to skip the version check."
    } >&2
    exit 1
  fi
  if ! version_ge "$SLANGC_VER" "$MIN_SLANGC"; then
    {
      echo "ERROR: slangc $SLANGC_VER is older than the $MIN_SLANGC release used"
      echo "for the committed reflection JSON in $REFL."
      echo ""
      echo "An older compiler emits the same offsets but drops schema fields"
      echo "(\"version\", \"combined\", \"sizes\", ...), so the generated bindings"
      echo "would silently stop matching the checked-in ones."
      echo ""
      echo "Install $MIN_SLANGC or newer: $SLANG_DOWNLOAD"
      echo "Override with SLANGC_MIN_VERSION=<version> (or 0 to skip) only if you"
      echo "know why the committed schema does not matter for your build."
    } >&2
    exit 1
  fi
fi

mkdir -p "$REFL"

# Scratch space for .spv and reflection output. Nothing lands in the repo until
# slangc succeeded, so a crashed compiler can never leave a truncated .spv that
# still looks shippable.
STAGE="$(mktemp -d "${TMPDIR:-/tmp}/prismrev-shaders.XXXXXX")"
FAILED_STAGE=""

cleanup() {
  local rc=$?
  rm -rf "$STAGE"
  if [ "$rc" -ne 0 ] && [ -n "$FAILED_STAGE" ]; then
    echo "" >&2
    echo "ERROR: shader compilation failed at: $FAILED_STAGE" >&2
    echo "The .spv files were NOT rebuilt by this run; anything left in $OUT is" >&2
    echo "from an older build and is stale — do not ship, measure or benchmark it." >&2
  fi
}
trap cleanup EXIT

# name : entry:stage pairs (space separated). stage = vert|frag (matches the
# .spv filenames the engine include_bytes!'s, e.g.
# mesh_vert.vert.spv / scene_frag.frag.spv / shadow_depth.vert.spv).
# Slang entry points are vertexMain / fragmentMain (see the [shader(...)] attrs).
compile_stage() {
  local name="$1" entry="$2" stage="$3" out_name="${4:-$1}"
  # Map slang stage names (vertex/fragment/compute) to the file extension the
  # engine expects (vert/frag/comp), so the generated .spv matches
  # include_bytes! paths.
  local ext
  case "$stage" in
    vertex)   ext=vert ;;
    fragment) ext=frag ;;
    compute)  ext=comp ;;
    *)        ext="$stage" ;;
  esac
  local out_spv="$OUT/${out_name}.${ext}.spv"
  local tmp_spv="$STAGE/${out_name}.${ext}.spv"
  echo "  $name :: $entry -> ${out_name}.${ext}.spv"
  if ! "$SLANGC" "$SRC/${name}.slang" \
    -profile "$PROFILE" \
    -target spirv \
    -entry "$entry" \
    -stage "$stage" \
    -fvk-use-entrypoint-name \
    -o "$tmp_spv"; then
    FAILED_STAGE="${name}.slang entry ${entry}"
    return 1
  fi
  mv -f "$tmp_spv" "$out_spv"
}

emit_reflection() {
  # One reflection JSON per module (covers all entry points + bindings).
  local name="$1"; shift
  local entries=()
  while [ "$#" -gt 0 ]; do entries+=(-entry "$1" -stage "$2"); shift 2; done
  echo "  reflect $name -> reflection/${name}.json"
  # slangc requires an SPIR-V output path even for reflection-only runs; the
  # throwaway module stays in the staging dir and disappears with it.
  if ! "$SLANGC" "$SRC/${name}.slang" \
    -profile "$PROFILE" \
    -target spirv \
    "${entries[@]}" \
    -reflection-json "$STAGE/${name}.json" \
    -o "$STAGE/${name}.reflect.spv"; then
    FAILED_STAGE="${name}.slang reflection"
    return 1
  fi
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

# Compare the freshly generated reflection with the committed files. Only
# --write-reflection copies the new files into the repo.
settle_reflection() {
  local staged name committed differing=0
  for staged in "$STAGE"/*.json; do
    [ -e "$staged" ] || continue
    name="$(basename "$staged")"
    committed="$REFL/$name"
    if [ -f "$committed" ] && cmp -s "$staged" "$committed"; then
      continue
    fi
    differing=$((differing + 1))
    if [ "$WRITE_REFLECTION" -eq 1 ]; then
      if [ -f "$committed" ]; then
        echo "  update reflection/$name"
      else
        echo "  add    reflection/$name"
      fi
      mkdir -p "$REFL"
      cp "$staged" "$committed"
    else
      if [ -f "$committed" ]; then
        echo "  differs reflection/$name" >&2
      else
        echo "  missing reflection/$name" >&2
      fi
    fi
  done

  if [ "$differing" -eq 0 ]; then
    echo "reflection: matches the committed JSON"
    return 0
  fi
  if [ "$WRITE_REFLECTION" -eq 1 ]; then
    echo "reflection: wrote $differing file(s) — rerun 'xtask shader-bindgen' and commit both" >&2
    return 0
  fi
  {
    echo ""
    echo "ERROR: $differing reflection file(s) differ from the committed JSON."
    echo "The compiler must be $MIN_SLANGC, the release that generated them."
    echo "Inspect with: git diff --stat -- assets/shaders/reflection"
    echo "If the schema change is intended (new push constants / bindings), rerun"
    echo "this script with --write-reflection, then regenerate and commit the Rust"
    echo "bindings: cd crates/xtask && cargo run --bin shader-bindgen -- \\"
    echo "  ../../assets/shaders/reflection ../../crates/prism-render/src/shader_bindings"
  } >&2
  return 1
}

echo "Compiling Slang shaders (slangc = $SLANGC / $SLANGC_VER, profile = $PROFILE)..."

# mesh_vert: vertex stage for the ForwardPass (shared by all scene geometry).
compile_stage mesh_vert vertexMain vertex
emit_reflection mesh_vert vertexMain vertex
# mesh_vert, skinned entry point: same module, extra vertex buffer
# (binding 1 = joints/weights, #560 P0-3).
compile_stage mesh_vert vertexSkinnedMain vertex mesh_vert_skinned

# outline: inverted-hull selection outline (#487). Vertex + solid-color fragment.
compile_stage outline vertexMain vertex
compile_stage outline fragmentMain fragment

# gizmo: vertex + fragment
compile_stage gizmo vertexMain vertex
compile_stage gizmo fragmentMain fragment
emit_reflection gizmo vertexMain vertex fragmentMain fragment

# shadow_depth: vertex + fragment (rasterized depth-only shadow map).
compile_stage shadow_depth vertexMain vertex
# shadow_depth, skinned entry point: same module, extra vertex buffer
# (binding 1 = joints/weights, #560 P0-3c).
compile_stage shadow_depth vertexSkinnedMain vertex shadow_depth_skinned
compile_stage shadow_depth fragmentMain fragment
emit_reflection shadow_depth vertexMain vertex fragmentMain fragment

# skybox: vertex + fragment (environment cubemap background).
# Single source file with two entry points (vertexMain / fragmentMain).
compile_stage skybox vertexMain vertex
compile_stage skybox fragmentMain fragment
emit_reflection skybox vertexMain vertex fragmentMain fragment

# TAA resolve (#883).
compile_stage taa vertexMain vertex
compile_stage taa fragmentMain fragment
emit_reflection taa vertexMain vertex fragmentMain fragment
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
compile_stage gi_bake bakeMain compute
fix_spv "$OUT/gi_bake.comp.spv"

# gi_render: compute shader for offline path-traced image rendering (ray-query).
# Dispatched per pixel — renders an HDR image directly. Requires
# VK_KHR_ray_query hardware. Output: RWTexture2D RGBA32F.
compile_stage gi_render renderMain compute
fix_spv "$OUT/gi_render.comp.spv"

# pt_render: real-time path tracing compute shader (temporal accumulation).
# Dispatched per pixel — traces 1 sample/frame, accumulates for convergence.
# Requires VK_KHR_ray_query hardware. Output: RWTexture2D RGBA32F (accumulated).
compile_stage pt_render ptMain compute
fix_spv "$OUT/pt_render.comp.spv"
emit_reflection pt_render ptMain compute

# harness_copy: tiny compute kernel for the reusable GPU shader harness test
# (`crates/prism-render/tests/shader_harness_gpu.rs`). No bindless state, so
# the SPIR-V needs no ArrayStride fix-up.
compile_stage harness_copy main compute

# harness_bench: grid-stride compute kernel for the best-of-N benchmark test
# (`crates/prism-render/tests/kernel_benchmark_gpu.rs`). Different group counts
# are all correct because the kernel strides by the dispatched thread count.
compile_stage harness_bench main compute

# ui_overlay: vertex + fragment (coloured quads on top of the scene output).
# The fragment stage samples the scene's bindless texture table for
# background images (#591), so Slang's illegal ArrayStride on bindless opaque
# runtime arrays applies here too (#580) — some slangc builds emit it.
compile_stage ui_overlay vertexMain vertex
compile_stage ui_overlay fragmentMain fragment
fix_spv "$OUT/ui_overlay.frag.spv"
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

settle_reflection

echo "All Slang shaders compiled. SPIR-V in $OUT, reflection JSON in $REFL"
