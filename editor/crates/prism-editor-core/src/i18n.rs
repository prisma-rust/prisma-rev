//! 界面文案（`editor/src/i18n.ts` + `locales/*` 的 Rust 移植）。
//!
//! 两份字典直接取自 Web 编辑器的 locale 源文件（JSON 由 TS 对象机械转换，
//! 键路径与文案保持一致），所以同一个 key 在两边显示同一句话。缺失的 key
//! 会回退到英文，再回退到 key 本身 —— 与前端 `t()` 的语义一致。

use serde_json::Value;
use std::collections::HashMap;

pub const ZH_CN: &str = include_str!("locales/zh-CN.json");
pub const EN_US: &str = include_str!("locales/en-US.json");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Locale {
    #[default]
    ZhCn,
    EnUs,
}

impl Locale {
    pub fn code(self) -> &'static str {
        match self {
            Locale::ZhCn => "zh-CN",
            Locale::EnUs => "en-US",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Locale::ZhCn => "简体中文",
            Locale::EnUs => "English",
        }
    }

    pub fn parse(code: &str) -> Locale {
        if code.starts_with("en") {
            Locale::EnUs
        } else {
            Locale::ZhCn
        }
    }

    /// 与前端一致：系统语言以 `en` 开头就用英文。
    pub fn detect(system_language: Option<&str>) -> Locale {
        match system_language {
            Some(language) if language.starts_with("en") => Locale::EnUs,
            _ => Locale::ZhCn,
        }
    }
}

#[derive(Debug, Clone)]
pub struct I18n {
    locale: Locale,
    dictionaries: [HashMap<String, String>; 2],
}

impl Default for I18n {
    fn default() -> Self {
        Self::new(Locale::default())
    }
}

impl I18n {
    pub fn new(locale: Locale) -> Self {
        Self {
            locale,
            dictionaries: [flatten(ZH_CN), flatten(EN_US)],
        }
    }

    pub fn locale(&self) -> Locale {
        self.locale
    }

    pub fn set_locale(&mut self, locale: Locale) {
        self.locale = locale;
    }

    fn dictionary(&self, locale: Locale) -> &HashMap<String, String> {
        match locale {
            Locale::ZhCn => &self.dictionaries[0],
            Locale::EnUs => &self.dictionaries[1],
        }
    }

    pub fn get(&self, path: &str) -> Option<&str> {
        self.dictionary(self.locale)
            .get(path)
            .or_else(|| self.dictionary(Locale::EnUs).get(path))
            .map(String::as_str)
    }

    /// `t("inspector.selectedCount", &[("count", "3")])`。
    pub fn t(&self, path: &str, params: &[(&str, &str)]) -> String {
        let template = self
            .get(path)
            .map(str::to_string)
            .unwrap_or_else(|| path.to_string());
        let mut out = template;
        for (key, value) in params {
            out = out.replace(&format!("{{{key}}}"), value);
        }
        out
    }

    pub fn t1(&self, path: &str) -> String {
        self.t(path, &[])
    }

    /// 字典里所有 key（调试 / 完整性测试用）。
    pub fn keys(&self) -> Vec<&str> {
        let mut keys: Vec<&str> = self
            .dictionary(self.locale)
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        keys
    }

    /// 两种语言的 key 集合必须一致，缺一条就说明移植漏了。
    pub fn missing_keys(&self) -> Vec<String> {
        let zh = self.dictionary(Locale::ZhCn);
        let en = self.dictionary(Locale::EnUs);
        let mut missing: Vec<String> = Vec::new();
        for key in zh.keys() {
            if !en.contains_key(key) {
                missing.push(key.clone());
            }
        }
        for key in en.keys() {
            if !zh.contains_key(key) {
                missing.push(key.clone());
            }
        }
        missing.sort_unstable();
        missing.dedup();
        missing
    }
}

fn flatten(json: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if let Ok(value) = serde_json::from_str::<Value>(json) {
        walk(&value, "", &mut out);
    }
    out
}

fn walk(value: &Value, prefix: &str, out: &mut HashMap<String, String>) {
    match value {
        Value::Object(map) => {
            for (key, item) in map {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                walk(item, &path, out);
            }
        }
        Value::String(text) => {
            out.insert(prefix.to_string(), text.clone());
        }
        other => {
            out.insert(prefix.to_string(), other.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_dictionaries_have_identical_keys() {
        let i18n = I18n::default();
        assert!(
            i18n.missing_keys().is_empty(),
            "locale 键不一致: {:?}",
            i18n.missing_keys()
        );
        assert!(i18n.keys().len() > 300, "字典条数 {}", i18n.keys().len());
    }

    #[test]
    fn translates_and_switches_locale() {
        let mut i18n = I18n::new(Locale::ZhCn);
        assert_eq!(i18n.t1("common.save"), "保存");
        i18n.set_locale(Locale::EnUs);
        assert_eq!(i18n.t1("common.save"), "Save");
    }

    #[test]
    fn substitutes_parameters() {
        let i18n = I18n::new(Locale::EnUs);
        let text = i18n.t("inspector.selectedCount", &[("count", "3")]);
        assert!(text.contains('3'), "{text}");
        assert!(!text.contains("{count}"));
    }

    #[test]
    fn unknown_key_falls_back_to_path() {
        let i18n = I18n::default();
        assert_eq!(i18n.t1("nope.nothing"), "nope.nothing");
    }

    #[test]
    fn detects_locale_like_frontend() {
        assert_eq!(Locale::detect(Some("en-GB")), Locale::EnUs);
        assert_eq!(Locale::detect(Some("zh-CN")), Locale::ZhCn);
        assert_eq!(Locale::detect(None), Locale::ZhCn);
        assert_eq!(Locale::parse("en-US"), Locale::EnUs);
    }
}
