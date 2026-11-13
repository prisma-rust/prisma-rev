//! Build-time gate for the SPIR-V modules `prism-render` embeds with `include_bytes!`.
//!
//! `assets/shaders/*.spv` is committed, so a clone builds without `slangc`. This
//! script keeps that honest: it runs before the crate is compiled and
//!   1. collects every `.spv` path referenced from `src/` (and `tests/`),
//!   2. recompiles them with `assets/shaders/compile.sh` when a `slangc` is
//!      available and the sources are newer (or a module is missing),
//!   3. otherwise fails once, with a message that can be replaced per project —
//!      without it, `rustc` reports one `couldn't read ... .spv` error per
//!      reference (over thirty of them) and never says what to do.
//!
//! Env knobs (all optional):
//!   `PRISMREV_SHADER_AUTOCOMPILE`  `0` disables running `compile.sh` (default: enabled)
//!   `PRISMREV_SHADER_REBUILD`      `1` always runs `compile.sh` (default: only when a
//!                                  `.spv` is missing or older than the `.slang` source)
//!   `PRISMREV_SHADER_SCRIPT`       `compile.sh` to run (default `<repo>/assets/shaders/compile.sh`)
//!   `PRISMREV_SHADER_HINT`         replaces the failure text
//!   `PRISMREV_SHADER_HINT_FILE`    file holding that text (wins over `PRISMREV_SHADER_HINT`)
//!   `SLANGC`                       handed to `compile.sh`
//!
//! Hint text may use the placeholders `{repo}`, `{shaders}` and `{missing}`.

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

fn main() {
    let manifest = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by cargo"),
    );
    let repo = manifest
        .ancestors()
        .nth(2)
        .map_or_else(|| manifest.clone(), Path::to_path_buf);
    let shaders = repo.join("assets").join("shaders");
    let script = env::var_os("PRISMREV_SHADER_SCRIPT")
        .map_or_else(|| shaders.join("compile.sh"), PathBuf::from);

    let runtime = scan(&manifest.join("src"));
    let test_only = scan(&manifest.join("tests"));

    let auto = env_flag("PRISMREV_SHADER_AUTOCOMPILE", true);
    let force = env_flag("PRISMREV_SHADER_REBUILD", false);
    let missing_now = missing_of(&runtime);
    let stale = missing_now.is_empty() && !force && slang_is_newer(&shaders, &runtime);

    let want_compile = !missing_now.is_empty() || force || stale;
    let mut note = None;
    if want_compile && !auto {
        note = Some("automatic compilation is off (PRISMREV_SHADER_AUTOCOMPILE=0)".to_string());
    } else if want_compile {
        if let Some(slangc) = find_slangc(&repo) {
            let what = if missing_now.is_empty() {
                "SPIR-V modules are older than the Slang sources"
            } else {
                "SPIR-V modules are missing"
            };
            eprintln!("prism-render: {what}, compiling with {}", slangc.display());
            match find_bash() {
                Some(bash) => match Command::new(bash)
                    .arg(&script)
                    .current_dir(&repo)
                    .env("SLANGC", &slangc)
                    .output()
                {
                    Ok(out) if out.status.success() => {
                        note = Some(format!("`{}` succeeded", script.display()));
                    }
                    Ok(out) => {
                        note = Some(format!(
                            "`{}` failed:\n{}",
                            script.display(),
                            tail(&out.stderr, 40)
                        ));
                    }
                    Err(err) => {
                        note = Some(format!("could not run `{}`: {err}", script.display()));
                    }
                },
                None => note = Some("no `bash` on PATH to run the shader script".to_string()),
            }
        } else if stale {
            println!(
                "cargo:warning=prism-render: Slang sources are newer than the committed SPIR-V \
                 and no `slangc` was found; regenerate them once with \
                 `bash {}/assets/shaders/compile.sh`, then commit the `.spv` with the `.slang`",
                repo.display()
            );
        }
    }

    let missing = missing_of(&runtime);
    if !missing.is_empty() {
        // Nothing goes to stdout on failure: cargo prints build-script stdout above the
        // error, and a watch list there would bury the message we want the reader to see.
        fail(&repo, &shaders, &missing, note.as_deref());
    }

    let missing_tests = missing_of(&test_only);
    if !missing_tests.is_empty() {
        println!(
            "cargo:warning=prism-render: {} test-only SPIR-V module(s) are missing ({}); \
             `cargo test` will fail until they are generated with `bash {}`",
            missing_tests.len(),
            names(&missing_tests),
            script.display()
        );
    }

    emit_watch_list(&manifest, &shaders, &script, &runtime, &test_only);
}

/// Declare everything that decides whether this script has to run again.
///
/// Printing any `rerun-if-changed` disables cargo's default "watch the whole package",
/// so the list is explicit: the sources we parse, the script, the `.spv` (their
/// directory catches creation/deletion, rustc's own dep-info catches content), and the
/// `.slang` sources so editing one re-runs this script and re-detects staleness.
fn emit_watch_list(
    manifest: &Path,
    shaders: &Path,
    script: &Path,
    runtime: &[PathBuf],
    test_only: &[PathBuf],
) {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src");
    if manifest.join("tests").is_dir() {
        println!("cargo:rerun-if-changed=tests");
    }
    println!("cargo:rerun-if-changed={}", shaders.display());
    println!("cargo:rerun-if-changed={}", script.display());
    for path in runtime.iter().chain(test_only.iter()) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    for path in slang_sources(shaders) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

fn fail(repo: &Path, shaders: &Path, missing: &[PathBuf], note: Option<&str>) -> ! {
    let missing_list = names(missing);
    let body = hint_text()
        .replace("{repo}", &repo.display().to_string())
        .replace("{shaders}", &shaders.display().to_string())
        .replace("{missing}", &missing_list);
    eprintln!(
        "prism-render: {} SPIR-V module(s) referenced by include_bytes! are missing.",
        missing.len()
    );
    eprintln!();
    eprintln!("{body}");
    if !body.contains(&missing_list) {
        eprintln!();
        eprintln!("  missing: {missing_list}");
        eprintln!("  expected in: {}", shaders.display());
    }
    if let Some(note) = note {
        eprintln!();
        eprintln!("  last attempt: {note}");
    }
    eprintln!();
    eprintln!("Re-run the build once the `.spv` files exist.");
    std::process::exit(1);
}

fn hint_text() -> String {
    if let Some(path) = env::var_os("PRISMREV_SHADER_HINT_FILE") {
        match fs::read_to_string(&path) {
            Ok(text) => return text.trim_end().to_string(),
            Err(err) => eprintln!(
                "cargo:warning=prism-render: cannot read PRISMREV_SHADER_HINT_FILE {}: {err}",
                Path::new(&path).display()
            ),
        }
    }
    if let Some(text) = env::var_os("PRISMREV_SHADER_HINT") {
        return text.to_string_lossy().trim_end().to_string();
    }
    DEFAULT_HINT.to_string()
}

const DEFAULT_HINT: &str = r#"The compiled Slang shaders are committed under `assets/shaders`. Either
restore them, or regenerate them after editing a `.slang`:

  A. files missing (fresh checkout, bad merge):
       git -C {repo} checkout -- assets/shaders

  B. after editing `assets/shaders/slang/*.slang` (needs slangc 2026.21+):
       bash {repo}/assets/shaders/compile.sh
       SLANGC=/path/to/slangc bash {repo}/assets/shaders/compile.sh

  C. no slangc at hand, need the CI bytecode:
       gh run download <run-id> -n spirv -D /tmp/spirv
       cp /tmp/spirv/*.spv {shaders}/

`cargo build` runs `compile.sh` on its own when it can find a `slangc`
(set PRISMREV_SHADER_AUTOCOMPILE=0 to turn that off)."#;

/// Collect every `.spv` path referenced from `include_bytes!` under `dir`.
fn scan(dir: &Path) -> Vec<PathBuf> {
    let mut found = BTreeSet::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                let Ok(source) = fs::read_to_string(&path) else {
                    continue;
                };
                let base = path.parent().unwrap_or(dir);
                for literal in spv_literals(&source) {
                    found.insert(normalize(&base.join(literal)));
                }
            }
        }
    }
    found.into_iter().collect()
}

/// String literals of `include_bytes!` calls that name a `.spv` file.
fn spv_literals(source: &str) -> Vec<&str> {
    const CALL: &str = "include_bytes!";
    let mut out = Vec::new();
    let mut rest = source;
    while let Some(at) = rest.find(CALL) {
        rest = &rest[at + CALL.len()..];
        let Some(open) = rest.find('"') else { break };
        let after = &rest[open + 1..];
        let Some(close) = after.find('"') else { break };
        let literal = &after[..close];
        // `tests/support/mod.rs` documents the pattern with an ellipsis placeholder.
        if literal.ends_with(".spv") && !literal.contains("...") {
            out.push(literal);
        }
        rest = &after[close + 1..];
    }
    out
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn missing_of(paths: &[PathBuf]) -> Vec<PathBuf> {
    paths.iter().filter(|p| !p.is_file()).cloned().collect()
}

fn names(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect::<Vec<_>>()
        .join(", ")
}

fn env_flag(key: &str, default: bool) -> bool {
    match env::var_os(key) {
        Some(value) => matches!(
            value.to_string_lossy().trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        None => default,
    }
}

fn find_slangc(repo: &Path) -> Option<PathBuf> {
    if let Some(path) = env::var_os("SLANGC") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    let local = repo
        .join("tools")
        .join("slang-dl")
        .join("bin")
        .join("slangc");
    for candidate in [local.clone(), local.with_extension("exe")] {
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    find_in_path(if cfg!(windows) {
        "slangc.exe"
    } else {
        "slangc"
    })
}

fn find_bash() -> Option<PathBuf> {
    find_in_path("bash")
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// True when any `.slang` source is newer than the oldest embedded `.spv`.
fn slang_is_newer(shaders: &Path, spv: &[PathBuf]) -> bool {
    let Some(newest_slang) = slang_sources(shaders).iter().filter_map(|p| mtime(p)).max() else {
        return false;
    };
    let Some(oldest_spv) = spv.iter().filter_map(|p| mtime(p)).min() else {
        return false;
    };
    newest_slang > oldest_spv
}

fn mtime(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).ok()?.modified().ok()
}

fn slang_sources(shaders: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![shaders.join("slang")];
    while let Some(current) = stack.pop() {
        let Ok(entries) = fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "slang") {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

fn tail(text: &[u8], lines: usize) -> String {
    let text = String::from_utf8_lossy(text);
    let collected: Vec<&str> = text.lines().collect();
    let start = collected.len().saturating_sub(lines);
    collected[start..].join("\n")
}
