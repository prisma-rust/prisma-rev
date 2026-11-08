//! 无头复刻编辑器的「构建项目」：`cargo build` → `build-scenes` → 拷产物。
//!
//! 用法（项目根是含 `assets/settings.ini` 的目录）：
//!
//! ```text
//! cargo run -p prism-editor-core --example player_build -- projects/fishing
//! ```
//!
//! 存在的意义：#989 这类「产物清单漏文件」的问题只有真的跑一遍发布布局才能发现，
//! 而在 UI 里点按钮没法进 CI。这个 example 走的是**编辑器同一份** [`plan_build`] /
//! [`copy_artifacts`]，所以它可以当作发布冒烟脚本。

use prism_editor_core::{build, settings::SettingsCache};
use std::path::PathBuf;
use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let Some(root) = std::env::args().nth(1).map(PathBuf::from) else {
        eprintln!("usage: player_build <project-root>");
        return ExitCode::from(2);
    };
    let root = match root.canonicalize() {
        Ok(root) => root,
        Err(error) => {
            eprintln!("project root {}: {error}", root.display());
            return ExitCode::FAILURE;
        }
    };

    let cache = SettingsCache::load(&root);
    if let Some(error) = &cache.error {
        eprintln!("settings.ini: {error}");
    }
    let plan = build::plan_build(&root, &cache.settings);

    for step in &plan.steps {
        println!("$ {} {}", step.program, step.args.join(" "));
        let status = match Command::new(&step.program)
            .args(&step.args)
            .current_dir(&step.cwd)
            .status()
        {
            Ok(status) => status,
            Err(error) => {
                eprintln!("{}: {error}", step.label);
                return ExitCode::FAILURE;
            }
        };
        if !status.success() {
            eprintln!("{} failed with {status}", step.label);
            return ExitCode::FAILURE;
        }
    }

    match build::copy_artifacts(&plan.copies) {
        Ok(copied) => {
            for line in &copied {
                println!("copied {line}");
            }
        }
        Err(error) => {
            eprintln!("copy artifacts: {error}");
            return ExitCode::FAILURE;
        }
    }

    // `required` 是**发布目录里的**可执行文件，拷完才有。
    for required in &plan.required {
        if !required.is_file() {
            eprintln!("missing build output: {}", required.display());
            return ExitCode::FAILURE;
        }
    }

    println!("player: {}", plan.output_dir.display());
    ExitCode::SUCCESS
}
