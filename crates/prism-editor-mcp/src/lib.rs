//! Testable core of the PrismaRev editor MCP server.
//!
//! The binary keeps the stdio loop and the process-spawning `game` / editor
//! hosts; this library exposes the transport types and document tools so
//! integration tests can drive the same dispatch path without a subprocess.

pub mod tools;

use std::path::PathBuf;

use serde_json::Value;

/// Static description of one MCP tool, as the tool modules build it.
///
/// The binary converts these into `rmcp`'s `Tool` when the client asks for
/// `tools/list`; keeping the table as plain data is what lets the tool modules
/// and the tests stay independent of the protocol layer.
#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
}

/// Resolves the project root from `--project <path>`, then `PRISMREV_PROJECT`,
/// then by walking up from the cwd looking for `assets/`.
pub fn resolve_root() -> Result<PathBuf, String> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--project" {
            let value = args.next().ok_or("--project 需要一个路径")?;
            return std::fs::canonicalize(&value)
                .map_err(|err| format!("项目根 '{value}' 不存在: {err}"));
        }
    }
    if let Some(value) = std::env::var_os("PRISMREV_PROJECT") {
        let value = PathBuf::from(value);
        return std::fs::canonicalize(&value).map_err(|err| {
            format!(
                "PRISMREV_PROJECT 指向的 '{}' 不存在: {err}",
                value.display()
            )
        });
    }
    prism_editor_api::scene::find_project_root()
}
