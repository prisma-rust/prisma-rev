//! Integration tests for the `prism-cook` 二进制 (std::process::Command).
//!
//! These tests build a minimal .pak, invoke the CLI binary, and verify its
//! stdout / stderr output — ensuring the validate-shortcut and metadata
//! features work as expected.

use std::path::{Path, PathBuf};
use std::process::Command;

use prism_asset::core::{AssetId, AssetType};
use prism_asset::package::PackageBuilder;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// 定位 the compiled prism-cook 二进制
///
/// `cargo test` places the test executable in `target/debug/deps/`, while the
/// CLI 二进制 lives in `target/debug/` (or `target/debug/prism-cook.exe` on
/// Windows).  We walk up from the test exe to find it.
fn cli_binary() -> PathBuf {
    let mut path = std::env::current_exe().expect("current exe");
    // Pop the binary name, then the "deps" directory if present.
    path.pop(); // binary name
    if path.ends_with("deps") {
        path.pop(); // deps
    }
    path.push(if cfg!(windows) {
        "prism-cook.exe"
    } else {
        "prism-cook"
    });
    if !path.exists() {
        // Fallback: maybe we're in a workspace target dir.
        path.pop();
        path.push("debug");
        path.push(if cfg!(windows) {
            "prism-cook.exe"
        } else {
            "prism-cook"
        });
    }
    assert!(path.exists(), "CLI binary not found: {}", path.display());
    path
}

/// Build a minimal valid .pak with a single asset.
fn build_minimal_pak(dir: &Path, name: &str) -> PathBuf {
    let id = AssetId::from_raw(0x1000_0001);
    let mut builder = PackageBuilder::new();
    builder.add_asset(id, AssetType::Binary, b"hello world".to_vec(), &[]);
    let pak_bytes = builder.build().expect("build minimal .pak");
    let pak_path = dir.join(name);
    std::fs::write(&pak_path, &pak_bytes).expect("write .pak");
    pak_path
}

/// Run the CLI binary with the given arguments, returning (stdout, stderr).
fn run_cli(args: &[&str]) -> (String, String) {
    let output = Command::new(cli_binary())
        .args(args)
        .output()
        .expect("failed to run prism-cook");

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    (stdout, stderr)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn cli_no_args_shows_help() {
    let (stdout, stderr) = run_cli(&[]);
    // Should show usage text on stdout.
    assert!(
        stdout.contains("Usage:"),
        "help text should contain Usage:\n{stdout}"
    );
    assert!(
        stdout.contains("prism-cook"),
        "help text should mention prism-cook\n{stdout}"
    );
    assert!(stderr.is_empty(), "no error expected on stderr:\n{stderr}");
}

#[test]
fn cli_nonexistent_pak_reports_error() {
    let (_stdout, stderr) = run_cli(&["C:/this/does/not/exist.pak"]);
    // The validate command returns an error → should appear on stderr.
    assert!(
        stderr.contains("error") || stderr.contains("Error"),
        "stderr should mention error:\n{stderr}"
    );
    // The binary should exit non-zero, but we only check output text.
}

#[test]
fn cli_validate_with_meta_json_shows_asset_names() {
    let dir = tempfile::tempdir().expect("temp dir");
    let pak_path = build_minimal_pak(dir.path(), "game.pak");

    // Write a .pak.meta.json alongside the .pak.
    let meta = serde_json::json!({
        "pak": "game.pak",
        "format": "RPAK",
        "version": 1,
        "asset_count": 1,
        "total_size": 123,
        "assets": [{
            "id": "0x10000001",
            "path": "my_asset.bin",
            "type": "binary",
            "importer": "raw-importer",
            "size": 11,
            "compressed_size": null,
            "compression_ratio": null
        }]
    });
    let meta_path = pak_path.with_extension("pak.meta.json");
    std::fs::write(&meta_path, serde_json::to_string_pretty(&meta).unwrap())
        .expect("write .pak.meta.json");

    let (stdout, stderr) = run_cli(&[&pak_path.to_string_lossy()]);

    assert!(stderr.is_empty(), "no error expected:\n{stderr}");
    assert!(stdout.contains("RPAK"), "should show magic:\n{stdout}");
    assert!(
        stdout.contains("my_asset.bin"),
        "should show asset name from metadata:\n{stdout}"
    );
    assert!(
        stdout.contains("binary"),
        "should show asset type:\n{stdout}"
    );
}

#[test]
fn cli_validate_without_meta_json_shows_fallback() {
    let dir = tempfile::tempdir().expect("temp dir");
    let pak_path = build_minimal_pak(dir.path(), "game.pak");
    // Intentionally *not* writing a .pak.meta.json.

    let (stdout, stderr) = run_cli(&[&pak_path.to_string_lossy()]);

    assert!(stderr.is_empty(), "no error expected:\n{stderr}");
    assert!(stdout.contains("RPAK"), "should show magic:\n{stdout}");
    // 回退 标签 should mention 二进制 and 资源
    assert!(
        stdout.contains("binary") && stdout.contains("asset"),
        "fallback should mention asset type:\n{stdout}"
    );
    // Tip about rebuilding should appear.
    assert!(
        stdout.contains("Tip / 提示"),
        "should show tip about rebuilding:\n{stdout}"
    );
}

#[test]
fn cli_validate_subcommand_works() {
    let dir = tempfile::tempdir().expect("temp dir");
    let pak_path = build_minimal_pak(dir.path(), "game.pak");

    let (stdout, stderr) = run_cli(&["validate", &pak_path.to_string_lossy()]);

    assert!(stderr.is_empty(), "no error expected:\n{stderr}");
    assert!(
        stdout.contains("RPAK"),
        "validate subcommand should show magic:\n{stdout}"
    );
    assert!(stdout.contains("1"), "should show 1 asset:\n{stdout}");
}

#[test]
fn cli_help_subcommand_shows_help() {
    let (stdout, stderr) = run_cli(&["help"]);
    assert!(
        stdout.contains("Usage:"),
        "help subcommand should show usage:\n{stdout}"
    );
    assert!(stderr.is_empty(), "no error on stderr:\n{stderr}");
}

#[test]
fn cli_import_and_build_texture_produces_valid_pak() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path();
    let root_str = root.to_string_lossy().to_string();

    // 1. init
    let (stdout, stderr) = run_cli(&["init", &root_str]);
    assert!(stderr.is_empty(), "init error: {stderr}");
    assert!(stdout.contains("initialized"), "init output: {stdout}");

    // 2. create a small PNG texture in Assets/
    let assets_dir = root.join("Assets");
    let img_path = assets_dir.join("test_tex.png");
    let img = image::RgbaImage::new(4, 4);
    img.save(&img_path).expect("save test png");

    // 3. scan
    let (_stdout, stderr) = run_cli(&["scan", "--project", &root_str]);
    assert!(stderr.is_empty(), "scan error: {stderr}");

    // 4. import
    let (_stdout, stderr) = run_cli(&["import", "--project", &root_str]);
    assert!(stderr.is_empty(), "import error: {stderr}");

    // Verify intermediate file was written into Library/Intermediate/
    let inter_dir = root.join("Library").join("Intermediate");
    assert!(inter_dir.exists(), "Library/Intermediate must exist");
    let inter_files: Vec<_> = std::fs::read_dir(&inter_dir)
        .expect("read inter_dir")
        .flatten()
        .collect();
    assert_eq!(inter_files.len(), 1, "should have 1 intermediate file");

    // 5. build
    let out_pak = root.join("out.pak");
    let out_pak_str = out_pak.to_string_lossy().to_string();
    let (_stdout, stderr) = run_cli(&["build", "--project", &root_str, "--output", &out_pak_str]);
    assert!(stderr.is_empty(), "build error: {stderr}");
    assert!(out_pak.exists(), "out.pak must be built");

    // 6. validate built pak
    let (v_stdout, v_stderr) = run_cli(&["validate", &out_pak_str]);
    assert!(v_stderr.is_empty(), "validate error: {v_stderr}");
    assert!(v_stdout.contains("RPAK"), "validate should show magic");
}

#[test]
fn cli_build_without_prior_import_imports_on_demand() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path();
    let root_str = root.to_string_lossy().to_string();

    // 1. init
    let (stdout, stderr) = run_cli(&["init", &root_str]);
    assert!(stderr.is_empty(), "init error: {stderr}");
    assert!(stdout.contains("initialized"), "init output: {stdout}");

    // 2. create texture
    let assets_dir = root.join("Assets");
    let img_path = assets_dir.join("test_tex.png");
    let img = image::RgbaImage::new(4, 4);
    img.save(&img_path).expect("save test png");

    // 3. scan
    let (_stdout, stderr) = run_cli(&["scan", "--project", &root_str]);
    assert!(stderr.is_empty(), "scan error: {stderr}");

    // 4. build directly without running import
    let out_pak = root.join("out.pak");
    let out_pak_str = out_pak.to_string_lossy().to_string();
    let (_stdout, stderr) = run_cli(&["build", "--project", &root_str, "--output", &out_pak_str]);
    assert!(stderr.is_empty(), "build error: {stderr}");
    assert!(out_pak.exists(), "out.pak must be built");

    // 5. validate
    let (v_stdout, v_stderr) = run_cli(&["validate", &out_pak_str]);
    assert!(v_stderr.is_empty(), "validate error: {v_stderr}");
    assert!(v_stdout.contains("RPAK"), "validate should show magic");
}

#[test]
fn cli_verify_reports_missing_and_orphan_intermediates() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path();
    let root_str = root.to_string_lossy().to_string();

    let (_stdout, stderr) = run_cli(&["init", &root_str]);
    assert!(stderr.is_empty(), "init error: {stderr}");

    let img_path = root.join("Assets").join("test_tex.png");
    image::RgbaImage::new(4, 4)
        .save(&img_path)
        .expect("save test png");

    let (_stdout, stderr) = run_cli(&["scan", "--project", &root_str]);
    assert!(stderr.is_empty(), "scan error: {stderr}");
    let (_stdout, stderr) = run_cli(&["import", "--project", &root_str]);
    assert!(stderr.is_empty(), "import error: {stderr}");

    // Intermediate names are content addressed instead of per-database serials.
    let inter_dir = root.join("Library").join("Intermediate");
    let names: Vec<String> = std::fs::read_dir(&inter_dir)
        .expect("read Intermediate/")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names.len(), 1, "one imported asset → one intermediate");
    assert!(
        names[0].ends_with(".bin") && names[0].split('-').count() == 3,
        "expected <source-hash>-<settings-hash>-v<importer-version>.bin, got: {}",
        names[0]
    );

    let (stdout, stderr) = run_cli(&["verify", "--project", &root_str]);
    assert!(stderr.is_empty(), "verify error: {stderr}");
    assert!(
        stdout.contains("verified"),
        "verify should pass on a fresh library:\n{stdout}"
    );

    // A source that changed without a re-import makes the record outdated.
    image::RgbaImage::new(8, 8)
        .save(&img_path)
        .expect("rewrite test png");
    let (stdout, stderr) = run_cli(&["verify", "--project", &root_str]);
    assert!(stdout.contains("outdated"), "verify output:\n{stdout}");
    assert!(
        stdout.contains("source changed"),
        "verify should explain why the record is outdated:\n{stdout}"
    );
    assert!(stderr.contains("out of date"), "verify error:\n{stderr}");

    // Re-importing repairs the record and the intermediate.
    let (_stdout, stderr) = run_cli(&["import", "--project", &root_str]);
    assert!(stderr.is_empty(), "re-import error: {stderr}");
    let (stdout, stderr) = run_cli(&["verify", "--project", &root_str]);
    assert!(stderr.is_empty(), "verify error: {stderr}");
    assert!(stdout.contains("verified"), "verify output:\n{stdout}");
    // The intermediate built from the old source hash is now unreferenced; it
    // stays on disk until a gc pass exists, and `verify` lists it as an orphan.
    assert!(
        stdout.contains("1 orphan"),
        "stale intermediate should be reported as an orphan:\n{stdout}"
    );

    // A missing intermediate is reported and fails the command.
    for entry in std::fs::read_dir(&inter_dir).expect("read Intermediate/") {
        std::fs::remove_file(entry.expect("intermediate entry").path())
            .expect("remove intermediate");
    }
    let (stdout, stderr) = run_cli(&["verify", "--project", &root_str]);
    assert!(stdout.contains("missing"), "verify output:\n{stdout}");
    assert!(stderr.contains("missing"), "verify error:\n{stderr}");

    // An intermediate no record points at is reported as an orphan.
    std::fs::write(inter_dir.join("orphan.bin"), b"stale").expect("write orphan");
    let (stdout, _stderr) = run_cli(&["verify", "--project", &root_str]);
    assert!(stdout.contains("orphan.bin"), "verify output:\n{stdout}");
}
