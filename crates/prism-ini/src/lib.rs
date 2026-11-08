//! The `assets/settings.ini` dialect: one parser for the engine, the editor
//! tools (`prism-editor-api`, used by the MCP server) and the editor data layer
//! (`prism-editor-core`).
//!
//! The dialect (see `docs/DESIGN.md` § "项目配置文件"; changing it means
//! changing that section):
//!
//! 1. `[section]` lines, then `key = value` inside the section; section order
//!    does not matter;
//! 2. whole-line comments start with `#` or `;` (leading whitespace ignored);
//!    **no inline comments** — a `#` / `;` inside a disk path never truncates
//!    the value;
//! 3. a value may be double quoted, with `\"`, `\\` and `\n` escapes inside;
//!    an unquoted value runs to the end of the line and is trimmed;
//! 4. the parser only produces `String`; typed conversion (u32 / i32 / f32 /
//!    bool) is the caller's job and logs a warning on a bad value;
//! 5. unknown sections and keys are the caller's business — there is no
//!    built-in schema, and [`Ini::render`] writes back everything it parsed;
//! 6. a repeated section is merged, a repeated key keeps the last value.
//!
//! No `toml` / `serde`: settings are a few dozen scalars, and a hand-written
//! parser keeps `toml_parser` / `winnow` / `toml_datetime` out of the runtime
//! dependency tree.

use std::fmt::Write as _;

/// One `[section]`: its name plus ordered `key = value` pairs, values already
/// unquoted but still strings.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IniSection {
    pub name: String,
    pub entries: Vec<(String, String)>,
}

impl IniSection {
    /// String value of `key`, or `None` when the section does not have it.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }

    /// Write a key: overwrite in place (keeping its position) or append.
    pub fn set(&mut self, key: &str, value: &str) {
        if let Some(slot) = self
            .entries
            .iter_mut()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value)
        {
            *slot = value.to_string();
        } else {
            self.entries.push((key.to_string(), value.to_string()));
        }
    }
}

/// A parsed configuration file: section order is file order, repeated sections
/// are already merged.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Ini {
    sections: Vec<IniSection>,
}

/// The editor crates call the document type [`IniDocument`]; it is the same
/// parser, so the two names are interchangeable.
pub type IniDocument = Ini;

impl Ini {
    /// Parse text. A malformed line only costs that line (skipped with a
    /// `log::debug!`): a broken settings file must still let the game start on
    /// defaults.
    pub fn parse(text: &str) -> Self {
        let mut ini = Self::default();
        // Index of the current section; `None` means no `[section]` seen yet,
        // so keys before the first header are dropped.
        let mut current: Option<usize> = None;

        for raw in text.lines() {
            let line = raw.trim_end_matches('\r').trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }

            if let Some(name) = section_header(line) {
                if name.is_empty() {
                    log::debug!("ini: empty section header on {raw:?}; ignored");
                    continue;
                }
                // A repeated section is merged: keep writing into the first one.
                current = Some(match ini.sections.iter().position(|s| s.name == name) {
                    Some(index) => index,
                    None => {
                        ini.sections.push(IniSection {
                            name: name.to_string(),
                            entries: Vec::new(),
                        });
                        ini.sections.len() - 1
                    }
                });
                continue;
            }

            let Some((key, value)) = line.split_once('=') else {
                log::debug!("ini: {raw:?} is neither a section nor `key = value`; ignored");
                continue;
            };
            let key = key.trim();
            if key.is_empty() {
                log::debug!("ini: {raw:?} has an empty key; ignored");
                continue;
            }
            let Some(index) = current else {
                log::debug!("ini: {raw:?} appears before any [section]; ignored");
                continue;
            };
            ini.sections[index].set(key, &parse_value(value));
        }

        ini
    }

    pub fn sections(&self) -> &[IniSection] {
        &self.sections
    }

    pub fn section(&self, name: &str) -> Option<&IniSection> {
        self.sections.iter().find(|section| section.name == name)
    }

    /// Raw string value (already unquoted); `None` when missing.
    pub fn get(&self, section: &str, key: &str) -> Option<&str> {
        self.section(section)?.get(key)
    }

    /// Text value as an owned `String`; the caller decides the default.
    pub fn get_string(&self, section: &str, key: &str) -> Option<String> {
        self.get(section, key).map(str::to_string)
    }

    pub fn get_u32(&self, section: &str, key: &str) -> Option<u32> {
        let raw = self.get(section, key)?;
        match raw.trim().parse::<u32>() {
            Ok(value) => Some(value),
            Err(_) => {
                log::warn!("settings: [{section}] {key} = {raw:?} is not a u32; using default");
                None
            }
        }
    }

    pub fn get_i32(&self, section: &str, key: &str) -> Option<i32> {
        let raw = self.get(section, key)?;
        match raw.trim().parse::<i32>() {
            Ok(value) => Some(value),
            Err(_) => {
                log::warn!("settings: [{section}] {key} = {raw:?} is not an i32; using default");
                None
            }
        }
    }

    pub fn get_f32(&self, section: &str, key: &str) -> Option<f32> {
        let raw = self.get(section, key)?;
        match raw.trim().parse::<f32>() {
            Ok(value) if value.is_finite() => Some(value),
            _ => {
                log::warn!("settings: [{section}] {key} = {raw:?} is not a number; using default");
                None
            }
        }
    }

    /// `true` / `false` (case-insensitive), plus `1` / `0` / `yes` / `no` /
    /// `on` / `off`.
    pub fn get_bool(&self, section: &str, key: &str) -> Option<bool> {
        let raw = self.get(section, key)?;
        match raw.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Some(true),
            "false" | "0" | "no" | "off" => Some(false),
            _ => {
                log::warn!("settings: [{section}] {key} = {raw:?} is not a bool; using default");
                None
            }
        }
    }

    /// Write a key, creating the section at the end when it does not exist.
    pub fn set(&mut self, section: &str, key: &str, value: &str) {
        let index = match self.sections.iter().position(|s| s.name == section) {
            Some(index) => index,
            None => {
                self.sections.push(IniSection {
                    name: section.to_string(),
                    entries: Vec::new(),
                });
                self.sections.len() - 1
            }
        };
        self.sections[index].set(key, value);
    }

    /// Normalise back to text: section order is insertion order, key order is
    /// write order.
    ///
    /// Only the value → text step happens here: nothing is sorted or dropped,
    /// so sections and keys the writer does not care about survive a round
    /// trip (comments do not — INI has no comment model that round-trips).
    pub fn render(&self) -> String {
        let mut out = String::new();
        for (index, section) in self.sections.iter().enumerate() {
            if index > 0 {
                out.push('\n');
            }
            let _ = writeln!(out, "[{}]", section.name);
            for (key, value) in &section.entries {
                let _ = writeln!(out, "{} = {}", key, format_value(value));
            }
        }
        out
    }
}

fn section_header(line: &str) -> Option<&str> {
    let inner = line.strip_prefix('[')?.strip_suffix(']')?;
    Some(inner.trim())
}

/// Unquote a value: `"..."` understands `\"` `\\` `\n`; an unquoted value runs
/// to the end of the line and is trimmed.
///
/// An unknown escape (a Windows path such as `C:\foo`) keeps the backslash
/// itself instead of swallowing the next character.
pub fn parse_value(raw: &str) -> String {
    let trimmed = raw.trim();
    let Some(body) = trimmed.strip_prefix('"') else {
        return trimmed.to_string();
    };

    let mut out = String::new();
    let mut chars = body.char_indices();
    let mut closed_at = None;
    while let Some((offset, ch)) = chars.next() {
        match ch {
            '\\' => match chars.next() {
                Some((_, 'n')) => out.push('\n'),
                Some((_, '"')) => out.push('"'),
                Some((_, '\\')) => out.push('\\'),
                Some((_, other)) => {
                    // Unknown escape: keep it, so `C:\temp` does not lose a char.
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            },
            '"' => {
                closed_at = Some(offset);
                break;
            }
            other => out.push(other),
        }
    }

    match closed_at {
        Some(offset) => {
            let rest = body[offset + 1..].trim();
            if !rest.is_empty() {
                log::debug!("ini: ignoring {rest:?} after the closing quote of {trimmed:?}");
            }
        }
        None => log::debug!("ini: unterminated quoted value {trimmed:?}; using it verbatim"),
    }
    out
}

/// Format a value for writing: numbers and booleans stay bare
/// (`width = 1600`, `vsync = false`), everything else is quoted
/// (`company = "Acme"`); anything that needs escaping, or has surrounding
/// whitespace, is quoted too.
///
/// A bare number is safe because the parser only produces strings: `"1234"`
/// and `1234` read back identically.
pub fn format_value(value: &str) -> String {
    let needs_quotes = value.is_empty()
        || value != value.trim()
        || value.contains('"')
        || value.contains('\\')
        || value.contains('\n')
        || value.contains('\r');
    if !needs_quotes {
        if looks_like_scalar(value) {
            return value.to_string();
        }
        return format!("\"{value}\"");
    }

    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

fn looks_like_scalar(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "true" | "false" | "yes" | "no" | "on" | "off"
    ) || value.parse::<i64>().is_ok()
        || value.parse::<f64>().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sections_and_keys() {
        let ini = Ini::parse("[window]\nwidth = 1600\nheight = 900\n\n[app]\ncompany = \"Acme\"\n");
        assert_eq!(ini.get("window", "width"), Some("1600"));
        assert_eq!(ini.get("window", "height"), Some("900"));
        assert_eq!(ini.get("app", "company"), Some("Acme"));
        assert_eq!(ini.get("app", "missing"), None);
        assert_eq!(ini.get("nope", "width"), None);
    }

    #[test]
    fn both_comment_styles_are_ignored_but_inline_hashes_are_values() {
        let ini = Ini::parse(
            "# a comment\n   ; indented comment\n\n[build]\noutput_dir = \"a#b;c\" # not a comment\n",
        );
        assert_eq!(ini.sections().len(), 1);
        // No inline comments: `#` / `;` inside quotes survive verbatim.
        assert_eq!(ini.get("build", "output_dir"), Some("a#b;c"));
    }

    #[test]
    fn inline_comment_markers_are_plain_value_characters() {
        let ini = Ini::parse("[build]\noutput_dir = build/win#1;x\n");
        assert_eq!(ini.get("build", "output_dir"), Some("build/win#1;x"));
    }

    #[test]
    fn comments_before_any_section_do_not_create_one() {
        let ini = Ini::parse("; header\n[render]\nrender_mode = raster\n");
        assert_eq!(ini.sections().len(), 1);
        assert_eq!(ini.sections()[0].name, "render");
    }

    #[test]
    fn quoted_values_unescape_and_unquoted_values_are_trimmed() {
        let ini = Ini::parse(
            "[app]\nname = \"say \\\"hi\\\"\"\npath = \"C:\\\\temp\\\\x\"\nline = \"a\\nb\"\nraw =   padded value   \n",
        );
        assert_eq!(ini.get("app", "name"), Some("say \"hi\""));
        assert_eq!(ini.get("app", "path"), Some("C:\\temp\\x"));
        assert_eq!(ini.get("app", "line"), Some("a\nb"));
        assert_eq!(ini.get("app", "raw"), Some("padded value"));
    }

    #[test]
    fn unknown_escape_keeps_the_backslash() {
        assert_eq!(parse_value("\"C:\\temp\""), "C:\\temp");
    }

    #[test]
    fn duplicate_sections_merge_and_last_key_wins() {
        let ini =
            Ini::parse("[app]\nname = \"one\"\n[window]\nwidth = 800\n[app]\nname = \"two\"\n");
        assert_eq!(ini.sections().len(), 2);
        assert_eq!(ini.sections()[0].entries.len(), 1);
        assert_eq!(ini.get("app", "name"), Some("two"));
        assert_eq!(ini.get("window", "width"), Some("800"));
    }

    #[test]
    fn empty_values_and_empty_quotes() {
        let ini = Ini::parse("[app]\nname =\ntitle = \"\"\n");
        assert_eq!(ini.get("app", "name"), Some(""));
        assert_eq!(ini.get("app", "title"), Some(""));
    }

    #[test]
    fn crlf_line_endings_are_supported() {
        let ini = Ini::parse("[window]\r\nwidth = 1280\r\ntitle = \"Win\"\r\n");
        assert_eq!(ini.get("window", "width"), Some("1280"));
        assert_eq!(ini.get("window", "title"), Some("Win"));
    }

    #[test]
    fn malformed_lines_are_skipped_without_losing_the_rest() {
        let ini = Ini::parse("stray = 1\n[window\nwidth = 640\n= 3\n[t]\nok = 1\n");
        // `[window` never closes, so it is a malformed line and `width` stays
        // before any section.
        assert_eq!(ini.get("t", "ok"), Some("1"));
        assert_eq!(ini.get("window", "width"), None);
    }

    #[test]
    fn typed_getters_convert_and_warn_on_bad_values() {
        let ini =
            Ini::parse("[window]\nwidth = 1280\nvsync = TRUE\nratio = 1.5\nbad = nope\nneg = -3\n");
        assert_eq!(ini.get_u32("window", "width"), Some(1280));
        assert_eq!(ini.get_bool("window", "vsync"), Some(true));
        assert_eq!(ini.get_f32("window", "ratio"), Some(1.5));
        assert_eq!(ini.get_u32("window", "bad"), None);
        assert_eq!(ini.get_i32("window", "neg"), Some(-3));
        assert_eq!(ini.get_string("window", "vsync").as_deref(), Some("TRUE"));
    }

    #[test]
    fn set_creates_sections_and_renders_round_trip() {
        let mut ini = Ini::parse("# note\n[app]\nname = \"Acme\"\n");
        ini.set("app", "version", "1.2.3");
        ini.set("window", "width", "1024");
        let text = ini.render();
        let reparsed = Ini::parse(&text);
        assert_eq!(reparsed.get("app", "name"), Some("Acme"));
        assert_eq!(reparsed.get("app", "version"), Some("1.2.3"));
        assert_eq!(reparsed.get("window", "width"), Some("1024"));
        assert!(!text.contains("# note"), "comments do not round trip");
    }

    #[test]
    fn unknown_sections_survive_a_round_trip() {
        // The editor writes only a few sections and must keep the rest.
        let mut ini = Ini::parse("[app]\ncompany = \"Acme\"\n[render]\nvsync = true\n");
        ini.set("app", "company", "Acme Labs");
        let reparsed = Ini::parse(&ini.render());
        assert_eq!(reparsed.get("render", "vsync"), Some("true"));
        assert_eq!(reparsed.get("app", "company"), Some("Acme Labs"));
    }

    #[test]
    fn render_quotes_only_when_needed() {
        assert_eq!(format_value("1600"), "1600");
        assert_eq!(format_value("false"), "false");
        assert_eq!(format_value("1.5"), "1.5");
        assert_eq!(format_value("Acme"), "\"Acme\"");
        assert_eq!(format_value(""), "\"\"");
        assert_eq!(format_value(" padded "), "\" padded \"");
        assert_eq!(format_value("a\"b"), "\"a\\\"b\"");
        assert_eq!(format_value("a\\b"), "\"a\\\\b\"");
        assert_eq!(format_value("a\nb"), "\"a\\nb\"");
        assert_eq!(
            Ini::parse(&format!("[x]\nk = {}\n", format_value("a\"b"))).get("x", "k"),
            Some("a\"b")
        );
    }

    #[test]
    fn the_document_alias_is_the_same_type() {
        let ini: IniDocument = IniDocument::parse("[a]\nk = v\n");
        assert_eq!(ini.get("a", "k"), Some("v"));
    }
}
