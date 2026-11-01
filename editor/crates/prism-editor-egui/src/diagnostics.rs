//! `cargo --message-format=json` 的解析结果。
//!
//! 项目模块（Game View 的 Play）和玩家构建（`cargo build`）都走这条路：
//! 编辑器只关心 error / warning + `file:line:col`，Problems 面板点一行就能跳转。
//! note / help / build-script 事件不是“要修的东西”，直接丢掉。

use std::path::{Path, PathBuf};

use prism_editor_core::LogLevel;

/// 一次最多保留多少条诊断，避免一个坏文件把面板刷爆。
pub const MAX_DIAGNOSTICS: usize = 100;

/// 一条编译器诊断。只保留 error / warning，位置是 `file:line:col`。
#[derive(Clone, Debug)]
pub struct Diagnostic {
    pub level: LogLevel,
    pub message: String,
    pub file: Option<PathBuf>,
    pub line: Option<usize>,
    pub column: Option<usize>,
}

impl Diagnostic {
    pub fn location(&self) -> Option<String> {
        let file = self.file.as_deref()?;
        let mut text = file.display().to_string();
        if let Some(line) = self.line {
            text.push_str(&format!(":{line}"));
            if let Some(column) = self.column {
                text.push_str(&format!(":{column}"));
            }
        }
        Some(text)
    }

    /// Console 的一行摘要（Console 不做富文本，位置放在消息前面更醒目）。
    pub fn console_line(&self) -> String {
        let level = match self.level {
            LogLevel::Error => "error",
            _ => "warning",
        };
        match self.location() {
            Some(location) => format!("{level}: {location}: {}", self.message),
            None => format!("{level}: {}", self.message),
        }
    }
}

/// 解析整段 stdout（一次性构建用）。
pub fn diagnostics_from_cargo(stdout: &str, root: &Path) -> Vec<Diagnostic> {
    stdout
        .lines()
        .filter_map(|line| parse_cargo_line(line, root))
        .take(MAX_DIAGNOSTICS)
        .collect()
}

/// 解析一行 cargo JSON（流式构建逐行调用）。不是编译诊断返回 `None`。
pub fn parse_cargo_line(line: &str, root: &Path) -> Option<Diagnostic> {
    let value = serde_json::from_str::<serde_json::Value>(line.trim()).ok()?;
    if value.get("reason").and_then(|reason| reason.as_str()) != Some("compiler-message") {
        return None;
    }
    let message = value.get("message")?;
    let level = match message.get("level").and_then(|level| level.as_str()) {
        Some("error") => LogLevel::Error,
        Some("warning") => LogLevel::Warn,
        _ => return None,
    };
    let mut text = message
        .get("message")
        .and_then(|text| text.as_str())
        .unwrap_or_default()
        .to_string();
    if let Some(code) = message
        .get("code")
        .and_then(|code| code.get("code"))
        .and_then(|code| code.as_str())
    {
        text = format!("{code}: {text}");
    }
    let span = message
        .get("spans")
        .and_then(|spans| spans.as_array())
        .and_then(|spans| {
            spans
                .iter()
                .find(|span| {
                    span.get("is_primary").and_then(|primary| primary.as_bool()) == Some(true)
                })
                .or_else(|| spans.first())
        });
    let file = span
        .and_then(|span| span.get("file_name"))
        .and_then(|name| name.as_str())
        .map(|name| {
            let path = Path::new(name);
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                root.join(path)
            }
        });
    Some(Diagnostic {
        level,
        message: text,
        file,
        line: span
            .and_then(|span| span.get("line_start"))
            .and_then(|line| line.as_u64())
            .map(|line| line as usize),
        column: span
            .and_then(|span| span.get("column_start"))
            .and_then(|column| column.as_u64())
            .map(|column| column as usize),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// cargo 的 JSON 诊断 → Problems 面板要的 file:line:col。
    #[test]
    fn cargo_diagnostics_carry_a_source_location() {
        let stdout = concat!(
            r#"{"reason":"compiler-message","message":{"level":"error","message":"mismatched types","code":{"code":"E0308"},"spans":[{"file_name":"src/game/world.rs","line_start":42,"column_start":9,"is_primary":true}]}}"#,
            "\n",
            r#"{"reason":"compiler-message","message":{"level":"note","message":"expected `f32`","spans":[]}}"#,
            "\n",
            r#"{"reason":"build-finished","success":false}"#,
            "\n",
            "not json at all",
        );
        let diagnostics = diagnostics_from_cargo(stdout, Path::new("/project"));
        assert_eq!(
            diagnostics.len(),
            1,
            "notes and cargo events are not problems"
        );
        let diagnostic = &diagnostics[0];
        assert_eq!(diagnostic.level, LogLevel::Error);
        assert_eq!(diagnostic.message, "E0308: mismatched types");
        assert_eq!(diagnostic.line, Some(42));
        assert_eq!(diagnostic.column, Some(9));
        assert!(
            diagnostic
                .file
                .as_deref()
                .is_some_and(|file| file.ends_with(Path::new("src/game/world.rs"))),
            "relative span paths are resolved against the project root"
        );
    }

    /// 编译成功时 warning 也要留下来：面板不能只有失败才有内容。
    #[test]
    fn warnings_survive_a_successful_build() {
        let stdout = r#"{"reason":"compiler-message","message":{"level":"warning","message":"unused variable: `state`","code":null,"spans":[{"file_name":"/abs/src/lib.rs","line_start":7,"column_start":13,"is_primary":true}]}}"#;
        let diagnostics = diagnostics_from_cargo(stdout, Path::new("/project"));
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].level, LogLevel::Warn);
        assert_eq!(
            diagnostics[0].file.as_deref(),
            Some(Path::new("/abs/src/lib.rs"))
        );
        assert_eq!(diagnostics[0].message, "unused variable: `state`");
    }

    /// 流式解析必须能处理半行噪声（cargo 偶尔夹带非 JSON 行）。
    #[test]
    fn streaming_parser_ignores_noise() {
        assert!(parse_cargo_line("", Path::new("/project")).is_none());
        assert!(parse_cargo_line("Compiling game v0.1.0", Path::new("/project")).is_none());
        assert!(parse_cargo_line(
            r#"{"reason":"build-script-executed","package_id":"game 0.1.0"}"#,
            Path::new("/project")
        )
        .is_none());
    }
}
