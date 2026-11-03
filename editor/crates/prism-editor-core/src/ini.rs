//! `assets/settings.ini` 的 INI 方言文档模型（解析 + 写回）。
//!
//! 方言与 `prism_engine::config::ini` / `prism_editor_api::ini` 完全一致
//! （见 `docs/DESIGN.md` §「项目配置文件」）：
//!
//! 1. `[section]` 行 + 节内 `key = value`；
//! 2. 整行注释 `#` / `;`（行内不解析注释，路径里的 `#` 不会被截断）；
//! 3. 值可加双引号，引号内支持 `\"` `\\` `\n`；不加引号时取到行尾并 trim；
//! 4. 解析结果只有 `String`，类型转换由调用方做；
//! 5. 同一 section 出现两次合并，后者覆盖前者。
//!
//! 编辑器写回要保留自己不管的段（`[render]` 等），所以 section / key 都按
//! 文件顺序保存。注释不在模型里（写回会丢），与之前 TOML 往返的行为一致。
//!
//! 本 crate 是独立 workspace 的数据层（不含引擎依赖），因此解析器在这里
//! 自带一份；改动方言时三处（engine / editor-api / 这里）要同步。

/// 一个 `[section]`。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IniSection {
    pub name: String,
    pub entries: Vec<(String, String)>,
}

impl IniSection {
    fn get(&self, key: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }

    fn set(&mut self, key: &str, value: &str) {
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

/// 一份解析后的配置文件。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IniDocument {
    sections: Vec<IniSection>,
}

impl IniDocument {
    pub fn parse(text: &str) -> Self {
        let mut document = Self::default();
        let mut current: Option<usize> = None;

        for raw in text.lines() {
            let line = raw.trim_end_matches('\r').trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            if let Some(name) = section_header(line) {
                if name.is_empty() {
                    continue;
                }
                current = Some(
                    match document.sections.iter().position(|s| s.name == name) {
                        Some(index) => index,
                        None => {
                            document.sections.push(IniSection {
                                name: name.to_string(),
                                entries: Vec::new(),
                            });
                            document.sections.len() - 1
                        }
                    },
                );
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let key = key.trim();
            let Some(index) = current else {
                continue;
            };
            if key.is_empty() {
                continue;
            }
            document.sections[index].set(key, &parse_value(value));
        }

        document
    }

    pub fn sections(&self) -> &[IniSection] {
        &self.sections
    }

    pub fn get(&self, section: &str, key: &str) -> Option<&str> {
        let section = self.sections.iter().find(|s| s.name == section)?;
        section.get(key)
    }

    pub fn get_string(&self, section: &str, key: &str) -> Option<String> {
        self.get(section, key).map(str::to_string)
    }

    pub fn get_u32(&self, section: &str, key: &str) -> Option<u32> {
        self.get(section, key)?.trim().parse().ok()
    }

    pub fn get_bool(&self, section: &str, key: &str) -> Option<bool> {
        match self.get(section, key)?.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Some(true),
            "false" | "0" | "no" | "off" => Some(false),
            _ => None,
        }
    }

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

    pub fn render(&self) -> String {
        let mut out = String::new();
        for (index, section) in self.sections.iter().enumerate() {
            if index > 0 {
                out.push('\n');
            }
            out.push('[');
            out.push_str(&section.name);
            out.push_str("]\n");
            for (key, value) in &section.entries {
                out.push_str(key);
                out.push_str(" = ");
                out.push_str(&format_value(value));
                out.push('\n');
            }
        }
        out
    }
}

fn section_header(line: &str) -> Option<&str> {
    let inner = line.strip_prefix('[')?.strip_suffix(']')?;
    Some(inner.trim())
}

/// 解引号（`\"` `\\` `\n`；未知转义保留反斜杠）。
pub fn parse_value(raw: &str) -> String {
    let trimmed = raw.trim();
    let Some(body) = trimmed.strip_prefix('"') else {
        return trimmed.to_string();
    };

    let mut out = String::new();
    let mut chars = body.char_indices();
    while let Some((_, ch)) = chars.next() {
        match ch {
            '\\' => match chars.next() {
                Some((_, 'n')) => out.push('\n'),
                Some((_, '"')) => out.push('"'),
                Some((_, '\\')) => out.push('\\'),
                Some((_, other)) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            },
            '"' => break,
            other => out.push(other),
        }
    }
    out
}

fn format_value(value: &str) -> String {
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

/// 数字 / 布尔裸写，其余字符串加引号——解析器只产出字符串，两种写法读回来一样。
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
    fn parses_sections_and_comments() {
        let document = IniDocument::parse(
            "; header\n[app]\ncompany = \"Acme\"\n# note\nname = PrismaRev\n[window]\nwidth = 1280\n",
        );
        assert_eq!(document.get("app", "company"), Some("Acme"));
        assert_eq!(document.get("app", "name"), Some("PrismaRev"));
        assert_eq!(document.get_u32("window", "width"), Some(1280));
    }

    #[test]
    fn duplicate_sections_merge() {
        let document = IniDocument::parse("[a]\nk = 1\n[b]\nk = 2\n[a]\nk = 3\n");
        assert_eq!(document.sections().len(), 2);
        assert_eq!(document.get("a", "k"), Some("3"));
    }

    #[test]
    fn quotes_and_crlf_round_trip() {
        let mut document = IniDocument::parse("[window]\r\ntitle = \"My \\\"Game\\\"\"\r\n");
        assert_eq!(document.get("window", "title"), Some("My \"Game\""));
        document.set("window", "vsync", "false");
        let reparsed = IniDocument::parse(&document.render());
        assert_eq!(reparsed.get("window", "title"), Some("My \"Game\""));
        assert_eq!(reparsed.get_bool("window", "vsync"), Some(false));
    }
}
