# Editor / Engine / Project Module Architecture

> **Status (2026-11-02): P0–P2 implemented; P3–P4 future / optional.**
>
> The editor is now the Rust + egui editor in `editor/`. References below to a
> Tauri/Vue backend describe the original design context and are historical.
> Current agent-facing rules live in `AGENTS.md` ("Project code runs as a
> dynamic module in Game View"); when this document disagrees with `AGENTS.md`
> or the code, those win.
>
> Goal: let the editor run the user's gameplay code in Play mode while keeping
> the engine reusable and keeping the runtime boundary explicit.

## Current implementation status

`prism-project-api` (C ABI) and `prism-runtime-host` load a same-build
`preview-module` cdylib, validate the versioned entry point, keep the module
alive safely, and drive an opaque `create -> tick -> shutdown` session from
Game View. The module does not receive `Engine`, `World`, Vulkan handles, or
Rust component values.

The current ABI (v10) exposes capability bits for logging, world commands,
components, drawing, and asset access. Components cross the boundary as RTTIBin
bytes through the host's `ComponentRegistry`, so adding a component type does
not change the ABI. The host table is tick-scoped: project code cannot retain a
world pointer across frames. Scene switching and raycast requests are recorded
and applied after the tick (`HOST_CAP_SCENE` / `HOST_CAP_PHYSICS`). Project logs
and panics are bridged by `install_host_logger` / `install_host_panic_hook`; a
failed tick latches until the module is reloaded.

P0–P2 of the phase list below are implemented. P3 (a separate engine dynamic
library) and P4 (a stable third-party SDK) remain future work and are not on the
current release path; standalone desktop/Android players stay monolithic.

## 1. Decision

The target runtime shape is:

```text
Editor executable
  -> Prism runtime host
       -> Prism engine dynamic library
            -> Project module dynamic library
```

The dependency direction is intentional, but the loading responsibility is
slightly more precise than "the engine scans for a game library":

- `editor` owns the preview session and starts/stops it;
- `prism-runtime-host` owns module discovery, ABI validation, and lifecycle;
- `prism-engine` owns ECS, rendering, assets, and simulation services;
- the project module owns project systems, resources, component registration,
  and gameplay setup;
- the project module must not know about Tauri, docking, or editor UI.

The engine may expose the project-module load operation through its runtime
API, but the host supplies the module path and owns the resulting session.
This keeps `prism-engine` independent from repository layout and project names.

## 2. Why change the current shape

The current editor already runs a hosted engine in-process:

```text
editor/src-tauri -> prism-app::App::hosted() -> prism-engine -> scene.json
```

This is the correct foundation for Scene View, offscreen rendering, pause,
step, and live document reload. It does not, however, execute user-project
Rust code such as `projects/prisma/src/lib.rs::build_app` or its registered
systems.

The project module adds that missing layer. Scene data remains data: unsaved
editor documents continue to be loaded from `target/editor-preview/`, while
the project module supplies behavior and project-specific registration.

## 3. Runtime layers

### 3.1 Editor

The editor (currently `editor/`, Rust + egui + ash; originally Tauri + Vue)
owns:

- project discovery and build commands;
- the preview process/session;
- Scene View and Game View state;
- document snapshots and editor-only camera/gizmo state;
- log and frame transport to the editor UI.

The editor must not link project gameplay crates directly. It requests a
preview session from the runtime host and receives opaque session handles and
diagnostics.

### 3.2 Prism runtime host

`prism-runtime-host` is the runtime-facing layer. Its responsibilities are:

- load the engine dynamic library;
- resolve and validate the engine API version;
- load the project module requested by the project manifest;
- resolve and validate the project API version;
- create one isolated preview session;
- route tick, input, scene reload, pause, step, and shutdown calls;
- unload the project module only after all project-owned resources are gone.

This layer is the dynamic-linking boundary. It should use `libloading` or a
small platform abstraction, but the rest of the editor must not call
`dlopen`/`LoadLibrary` directly.

### 3.3 Engine dynamic library

The first engine dynamic library should contain the runtime implementation
behind a narrow exported API. It must not export Rust implementation types
such as `World`, `App`, `Entity`, `Vec`, or trait objects.

The engine library owns:

- `EngineSession` state;
- ECS world and schedules;
- scene and asset loading;
- render host integration;
- input and timing services;
- engine-side component/type registration.

The exported API returns opaque handles and uses caller-provided buffers or
callbacks for data crossing the boundary.

### 3.4 Project module

The project module is built from the user's project and contains:

- gameplay systems;
- project resources;
- project component registration;
- project-specific startup/shutdown hooks;
- optional project render settings and input bindings.

It must not export a complete `prism_app::App` as it does today. The current
`build_app() -> prism_app::App` remains a convenient native entry point for
the standalone desktop and Android targets, but the module entry point uses a
separate ABI adapter.

## 4. ABI strategy

There are two compatibility levels.

### Phase 1: same-build module ABI

The first implementation may require all three artifacts to be produced by
the same Cargo build profile, Rust toolchain, engine revision, and feature
set. This keeps iteration practical while the API is still changing.

Even in this phase, the boundary must use an explicit `#[repr(C)]` function
table and opaque pointers. Do not pass Rust structs with unspecified layout,
Rust strings, `Vec`, slices, ECS entities, or trait objects across the module
boundary.

Example shape:

```rust
#[repr(C)]
pub struct PrismProjectApi {
    pub abi_version: u32,
    pub engine_abi_min: u32,
    pub engine_abi_max: u32,
    pub project_name: unsafe extern "C" fn(*mut PrismBuffer) -> PrismStatus,
    pub create: unsafe extern "C" fn(*const PrismHostApi,
                                      *mut *mut PrismProject) -> PrismStatus,
    pub tick: unsafe extern "C" fn(*mut PrismProject, f32) -> PrismStatus,
    pub shutdown: unsafe extern "C" fn(*mut PrismProject) -> PrismStatus,
}

#[no_mangle]
pub unsafe extern "C" fn prism_project_get_api(
    host: *const PrismHostApi,
    out: *mut PrismProjectApi,
) -> PrismStatus;
```

The exact names are provisional. The important properties are versioned
function tables, opaque state, explicit status returns, and one owner for
allocation/deallocation.

### Phase 2: versioned SDK ABI

When third-party modules or independent release cadence is required, freeze a
small SDK crate containing only C-compatible DTOs and function tables. Add:

- ABI major/minor version negotiation;
- engine build identifier and feature bits;
- allocator and buffer ownership rules;
- UTF-8 string encoding and lifetime rules;
- error retrieval into caller-owned buffers;
- thread-affinity requirements;
- capability negotiation for optional services.

The SDK must not expose the internal ECS storage layout. Project code should
use host functions for registration and commands, or be compiled against a
strictly version-matched engine SDK.

## 5. Project host API

The project module needs controlled access to engine services. The host API is
split into capabilities rather than exposing the whole engine:

- `project_registration`: component/type/resource registration;
- `world_commands`: spawn, despawn, component access, and event emission;
- `scene_services`: current scene identity and reload notifications;
- `input_services`: action and raw input queries;
- `render_settings`: project-level render configuration;
- `logging`: structured log emission;
- `time`: fixed-step and frame timing;
- `assets`: typed handles or path queries through the runtime asset layer.

Editor-only services are excluded. Selection, gizmos, dock panels, document
undo, and Tauri commands never cross into the project module.

## 6. Preview lifecycle

The editor should manage a preview session with this lifecycle:

```text
discover project
  -> build/check engine + project module
  -> load engine library
  -> load project module
  -> validate ABI and capabilities
  -> create EngineSession
  -> project.create(host_api)
  -> load current scene snapshot
  -> start hosted rendering
  -> tick / pause / step / input
  -> reload scene snapshot
  -> project.shutdown
  -> destroy EngineSession
  -> unload project module
```

Scene reload must not unload the project module. It replaces the session world
and re-runs the documented scene-load hooks. This preserves gameplay code and
avoids reloading native code for every inspector edit.

Play should use a separate simulation world from the editor document state:

- Scene View edits the authoring document and its editor preview world;
- Game View Play owns a runtime world created from a disk snapshot;
- stopping Play destroys the runtime world and restores the editor preview;
- project module global state must be reset by `create`/`shutdown`, not left in
  process-global mutable singletons.

## 7. Standalone desktop and Android

The dynamic module is primarily an editor/desktop preview mechanism. The
shipping game does not need to perform runtime plugin discovery on Android.

### Desktop

The standalone executable may either:

1. statically link the project adapter and engine for the simplest release
   path; or
2. use the same runtime host and project module layout as the editor.

The second option is useful for parity testing, but it is not required for the
first implementation.

### Android

Keep the existing GameActivity contract:

```text
GameActivity -> libprisma.so -> project entry -> prism-app / prism-engine
```

The project adapter can be statically linked into `libprisma.so`. Android
should not depend on arbitrary filesystem `.so` discovery unless a later
plugin marketplace or modding requirement justifies the packaging and
security cost.

## 8. Build and packaging

> **Current state:** the manifest sketch below is historical. The implemented
> flow builds `projects/<p>` with `--no-default-features --features
> preview-module` into `projects/<p>/target/preview-module/` and stages the
> cdylib before loading it; see `AGENTS.md`. The principles below still hold:
> same-profile modules, absolute paths, and explicit diagnostics.

Add a project manifest section describing the module contract, for example:

```toml
[project]
name = "prisma"

[preview]
module = "target/debug/libprisma_project.so"
engine = "target/debug/libprism_runtime.so"
abi = 1
```

The editor build command should:

1. resolve the selected project manifest;
2. build the engine runtime and project module with the same profile;
3. validate that the expected library exists;
4. start the preview host with absolute paths;
5. report missing or incompatible modules in the editor log panel.

The module path must never be inferred from `PATH`. The existing launcher rule
that resolves a known game executable before spawning is the right precedent.

## 9. Failure and reload rules

- An ABI mismatch prevents the preview session from starting and shows the
  expected/actual versions.
- A project `tick` error stops simulation but leaves the editor usable.
- A module load failure must not crash the editor process.
- A failed hot reload destroys the project session and starts a clean session;
  it must not leave old function pointers active.
- The engine library is unloaded only after renderer shutdown, asset uploads,
  project shutdown, and all project callbacks have completed.
- No engine or project library may retain callbacks into an unloaded module.

## 10. Implementation phases

### P0: contract and split — implemented

- Extract project registration from `build_app()` into a project adapter module.
- Keep standalone desktop and Android behavior unchanged.
- Define `PrismProjectApi`, `PrismHostApi`, status codes, and ownership rules.
- Add ABI validation tests without dynamic loading.

### P1: engine runtime host — implemented

The host loads the project module, drives its opaque session, and integrates it
with the hosted engine through `EngineHost`.

- Introduce the runtime host abstraction behind the current hosted viewport.
- Move viewport code from directly constructing project-agnostic `App::hosted()`
  to requesting a host session.
- Load a same-build project module on desktop.
- Run project startup, tick, pause, step, input, and shutdown callbacks.

### P2: editor Play integration — implemented

- Make Game View use the project module session.
- Preserve current unsaved scene snapshot and reload behavior.
- Route project logs into the existing editor/game log streams.
- Add module diagnostics to the editor status panel.

### P3: optional engine dynamic library — not implemented

- Split the engine runtime implementation from the host-facing API.
- Build `libprism_runtime` for supported desktop platforms.
- Ensure the editor has exactly one engine instance and does not also statically
  link the same engine implementation.
- Add unload and repeated start/stop tests.

### P4: stable SDK, if needed — not implemented

- Freeze the C-compatible SDK only after the same-build flow is exercised.
- Add compatibility ranges, feature negotiation, and independent module build
  documentation.

## 11. Non-goals

- Do not make `prism-engine` scan arbitrary project directories.
- Do not expose Rust `World` or Vulkan handles through the plugin ABI.
- Do not make Android load editor-style project modules from writable storage.
- Do not couple project modules to a specific editor UI or web framework.
- Do not require dynamic loading for the first standalone game release.
- Do not reload native project code for ordinary scene document edits.

## 12. Acceptance criteria

The same-build desktop path satisfies criteria 1–6 as of 2026-11. The optional
engine dynamic library (P3) and stable SDK (P4) are not required for this.

The design is considered implemented when all of the following are true:

1. Editor Play executes a project-defined system that is absent from the
   generic engine.
2. Scene View continues to work with unsaved `.scene.json` snapshots.
3. Play can pause, step, stop, and restart without leaking project state.
4. A mismatched project module is rejected before any gameplay callback runs.
5. The editor process remains usable after module load, tick, or reload errors.
6. Desktop, Android, and editor entry points share project behavior without
   making `prism-engine` depend on a concrete project name or path.
