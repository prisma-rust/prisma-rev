//! 编辑器主题（`editor/src/theme.ts` 的 Rust 移植）。
//!
//! 主题只描述**颜色**：每个 token 对应一个具名颜色，UI 一律通过 `Theme::color`
//! 取值，不再写死颜色。预设与 Web 编辑器完全一致（同一个预设 id 在两边看起来一样）。

use std::collections::HashMap;

/// sRGB 颜色（8 bit 分量）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    /// `#rrggbb`（可带 `#rrggbbaa`）。非法输入回退到不透明黑。
    pub fn from_hex(hex: &str) -> Self {
        let hex = hex.trim().trim_start_matches('#');
        let parse = |range: std::ops::Range<usize>| {
            u8::from_str_radix(hex.get(range).unwrap_or("00"), 16).unwrap_or(0)
        };
        match hex.len() {
            6 => Self::new(parse(0..2), parse(2..4), parse(4..6), 255),
            8 => Self::new(parse(0..2), parse(2..4), parse(4..6), parse(6..8)),
            _ => Self::new(0, 0, 0, 255),
        }
    }

    pub fn opacify(self, alpha: f32) -> Self {
        Self {
            a: (alpha.clamp(0.0, 1.0) * 255.0).round() as u8,
            ..self
        }
    }

    /// 线性插值（用于 hover / 选中态的派生色）。
    pub fn mix(self, other: Self, t: f32) -> Self {
        let t = t.clamp(0.0, 1.0);
        let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
        Self::new(
            mix(self.r, other.r),
            mix(self.g, other.g),
            mix(self.b, other.b),
            mix(self.a, other.a),
        )
    }

    pub fn to_hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }
}

/// 所有可主题化的颜色 token（顺序与 `THEME_TOKEN_GROUPS` 一致）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(usize)]
pub enum ThemeToken {
    Bg,
    BgSunken,
    Panel,
    PanelAlt,
    PanelRaised,
    Chrome,
    Border,
    BorderStrong,
    BorderSubtle,
    Text,
    TextStrong,
    TextSoft,
    Muted,
    MutedDim,
    RowHover,
    MenuHover,
    MenuActive,
    Selection,
    Accent,
    AccentSolid,
    OnAccent,
    AccentInk,
    AccentAlt,
    Success,
    SuccessSoft,
    SuccessBg,
    SuccessBorder,
    Danger,
    DangerSoft,
    DangerBg,
    DangerBgSubtle,
    DangerSolid,
    DangerBorder,
    OnDanger,
    Warning,
    WarningStrong,
    WarningBorder,
    Panic,
}

impl ThemeToken {
    pub const COUNT: usize = 38;

    pub const ALL: [ThemeToken; Self::COUNT] = [
        ThemeToken::Bg,
        ThemeToken::BgSunken,
        ThemeToken::Panel,
        ThemeToken::PanelAlt,
        ThemeToken::PanelRaised,
        ThemeToken::Chrome,
        ThemeToken::Border,
        ThemeToken::BorderStrong,
        ThemeToken::BorderSubtle,
        ThemeToken::Text,
        ThemeToken::TextStrong,
        ThemeToken::TextSoft,
        ThemeToken::Muted,
        ThemeToken::MutedDim,
        ThemeToken::RowHover,
        ThemeToken::MenuHover,
        ThemeToken::MenuActive,
        ThemeToken::Selection,
        ThemeToken::Accent,
        ThemeToken::AccentSolid,
        ThemeToken::OnAccent,
        ThemeToken::AccentInk,
        ThemeToken::AccentAlt,
        ThemeToken::Success,
        ThemeToken::SuccessSoft,
        ThemeToken::SuccessBg,
        ThemeToken::SuccessBorder,
        ThemeToken::Danger,
        ThemeToken::DangerSoft,
        ThemeToken::DangerBg,
        ThemeToken::DangerBgSubtle,
        ThemeToken::DangerSolid,
        ThemeToken::DangerBorder,
        ThemeToken::OnDanger,
        ThemeToken::Warning,
        ThemeToken::WarningStrong,
        ThemeToken::WarningBorder,
        ThemeToken::Panic,
    ];

    pub fn index(self) -> usize {
        self as usize
    }

    pub fn key(self) -> &'static str {
        match self {
            ThemeToken::Bg => "bg",
            ThemeToken::BgSunken => "bgSunken",
            ThemeToken::Panel => "panel",
            ThemeToken::PanelAlt => "panelAlt",
            ThemeToken::PanelRaised => "panelRaised",
            ThemeToken::Chrome => "chrome",
            ThemeToken::Border => "border",
            ThemeToken::BorderStrong => "borderStrong",
            ThemeToken::BorderSubtle => "borderSubtle",
            ThemeToken::Text => "text",
            ThemeToken::TextStrong => "textStrong",
            ThemeToken::TextSoft => "textSoft",
            ThemeToken::Muted => "muted",
            ThemeToken::MutedDim => "mutedDim",
            ThemeToken::RowHover => "rowHover",
            ThemeToken::MenuHover => "menuHover",
            ThemeToken::MenuActive => "menuActive",
            ThemeToken::Selection => "selection",
            ThemeToken::Accent => "accent",
            ThemeToken::AccentSolid => "accentSolid",
            ThemeToken::OnAccent => "onAccent",
            ThemeToken::AccentInk => "accentInk",
            ThemeToken::AccentAlt => "accentAlt",
            ThemeToken::Success => "success",
            ThemeToken::SuccessSoft => "successSoft",
            ThemeToken::SuccessBg => "successBg",
            ThemeToken::SuccessBorder => "successBorder",
            ThemeToken::Danger => "danger",
            ThemeToken::DangerSoft => "dangerSoft",
            ThemeToken::DangerBg => "dangerBg",
            ThemeToken::DangerBgSubtle => "dangerBgSubtle",
            ThemeToken::DangerSolid => "dangerSolid",
            ThemeToken::DangerBorder => "dangerBorder",
            ThemeToken::OnDanger => "onDanger",
            ThemeToken::Warning => "warning",
            ThemeToken::WarningStrong => "warningStrong",
            ThemeToken::WarningBorder => "warningBorder",
            ThemeToken::Panic => "panic",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ThemeToken::Bg => "应用背景",
            ThemeToken::BgSunken => "凹陷区域（日志/输入）",
            ThemeToken::Panel => "面板",
            ThemeToken::PanelAlt => "次级面板（卡头/tab）",
            ThemeToken::PanelRaised => "浮层（菜单/弹窗）",
            ThemeToken::Chrome => "菜单栏/状态栏",
            ThemeToken::Border => "边框",
            ThemeToken::BorderStrong => "强边框/分隔线",
            ThemeToken::BorderSubtle => "弱边框",
            ThemeToken::Text => "正文",
            ThemeToken::TextStrong => "高亮正文/标题",
            ThemeToken::TextSoft => "次级正文",
            ThemeToken::Muted => "弱化文本",
            ThemeToken::MutedDim => "最弱文本（提示）",
            ThemeToken::RowHover => "行悬停底色",
            ThemeToken::MenuHover => "菜单悬停底色",
            ThemeToken::MenuActive => "菜单展开/按下底色",
            ThemeToken::Selection => "选中行底色",
            ThemeToken::Accent => "强调色",
            ThemeToken::AccentSolid => "实心强调块",
            ThemeToken::OnAccent => "实心强调块上的文字",
            ThemeToken::AccentInk => "渐变按钮上的文字",
            ThemeToken::AccentAlt => "第二强调色",
            ThemeToken::Success => "成功/运行指示",
            ThemeToken::SuccessSoft => "成功文字",
            ThemeToken::SuccessBg => "成功底色（Play）",
            ThemeToken::SuccessBorder => "成功边框",
            ThemeToken::Danger => "危险/错误",
            ThemeToken::DangerSoft => "危险文字",
            ThemeToken::DangerBg => "危险底色（Stop）",
            ThemeToken::DangerBgSubtle => "危险悬停底色",
            ThemeToken::DangerSolid => "危险实心块",
            ThemeToken::DangerBorder => "危险边框",
            ThemeToken::OnDanger => "危险块上的文字",
            ThemeToken::Warning => "警告",
            ThemeToken::WarningStrong => "警告强调",
            ThemeToken::WarningBorder => "警告边框",
            ThemeToken::Panic => "panic 日志",
        }
    }
}

/// 主题面板的分组（顺序即展示顺序）。
pub struct TokenGroup {
    pub id: &'static str,
    pub label: &'static str,
    pub tokens: &'static [ThemeToken],
}

pub const TOKEN_GROUPS: &[TokenGroup] = &[
    TokenGroup {
        id: "surface",
        label: "背景与表面",
        tokens: &[
            ThemeToken::Bg,
            ThemeToken::BgSunken,
            ThemeToken::Panel,
            ThemeToken::PanelAlt,
            ThemeToken::PanelRaised,
            ThemeToken::Chrome,
        ],
    },
    TokenGroup {
        id: "border",
        label: "边框与分隔",
        tokens: &[
            ThemeToken::Border,
            ThemeToken::BorderStrong,
            ThemeToken::BorderSubtle,
        ],
    },
    TokenGroup {
        id: "text",
        label: "文本",
        tokens: &[
            ThemeToken::Text,
            ThemeToken::TextStrong,
            ThemeToken::TextSoft,
            ThemeToken::Muted,
            ThemeToken::MutedDim,
        ],
    },
    TokenGroup {
        id: "interaction",
        label: "交互状态",
        tokens: &[
            ThemeToken::RowHover,
            ThemeToken::MenuHover,
            ThemeToken::MenuActive,
            ThemeToken::Selection,
        ],
    },
    TokenGroup {
        id: "accent",
        label: "强调色",
        tokens: &[
            ThemeToken::Accent,
            ThemeToken::AccentSolid,
            ThemeToken::OnAccent,
            ThemeToken::AccentInk,
            ThemeToken::AccentAlt,
        ],
    },
    TokenGroup {
        id: "status",
        label: "状态色",
        tokens: &[
            ThemeToken::Success,
            ThemeToken::SuccessSoft,
            ThemeToken::SuccessBg,
            ThemeToken::SuccessBorder,
            ThemeToken::Danger,
            ThemeToken::DangerSoft,
            ThemeToken::DangerBg,
            ThemeToken::DangerBgSubtle,
            ThemeToken::DangerSolid,
            ThemeToken::DangerBorder,
            ThemeToken::OnDanger,
            ThemeToken::Warning,
            ThemeToken::WarningStrong,
            ThemeToken::WarningBorder,
            ThemeToken::Panic,
        ],
    },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeMode {
    Dark,
    Light,
}

impl ThemeMode {
    pub fn is_dark(self) -> bool {
        matches!(self, ThemeMode::Dark)
    }
}

/// 一套完整主题 = 每个 token 一个颜色。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ThemeColors {
    values: [Rgba; ThemeToken::COUNT],
}

impl ThemeColors {
    pub fn color(&self, token: ThemeToken) -> Rgba {
        self.values[token.index()]
    }

    pub fn set(&mut self, token: ThemeToken, color: Rgba) {
        self.values[token.index()] = color;
    }

    pub fn hexes(&self) -> Vec<String> {
        self.values.iter().map(|color| color.to_hex()).collect()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ThemePreset {
    pub id: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub mode: ThemeMode,
    pub colors: ThemeColors,
}

pub const THEME_PRESETS: &[ThemePreset] = &[
    ThemePreset {
        id: "studio",
        label: "工业暗色",
        description: "默认 · 工业级经典暗色中性灰",
        mode: ThemeMode::Dark,
        colors: ThemeColors {
            values: [
                Rgba::new(28, 28, 28, 255),    // bg
                Rgba::new(24, 24, 24, 255),    // bgSunken
                Rgba::new(40, 40, 40, 255),    // panel
                Rgba::new(48, 48, 48, 255),    // panelAlt
                Rgba::new(56, 56, 56, 255),    // panelRaised
                Rgba::new(34, 34, 34, 255),    // chrome
                Rgba::new(26, 26, 26, 255),    // border
                Rgba::new(53, 53, 53, 255),    // borderStrong
                Rgba::new(36, 36, 36, 255),    // borderSubtle
                Rgba::new(196, 196, 196, 255), // text
                Rgba::new(255, 255, 255, 255), // textStrong
                Rgba::new(171, 171, 171, 255), // textSoft
                Rgba::new(118, 118, 118, 255), // muted
                Rgba::new(88, 88, 88, 255),    // mutedDim
                Rgba::new(48, 48, 48, 255),    // rowHover
                Rgba::new(58, 58, 58, 255),    // menuHover
                Rgba::new(44, 93, 136, 255),   // menuActive
                Rgba::new(44, 93, 136, 255),   // selection
                Rgba::new(58, 117, 196, 255),  // accent
                Rgba::new(44, 93, 136, 255),   // accentSolid
                Rgba::new(255, 255, 255, 255), // onAccent
                Rgba::new(10, 20, 32, 255),    // accentInk
                Rgba::new(86, 156, 214, 255),  // accentAlt
                Rgba::new(78, 201, 176, 255),  // success
                Rgba::new(143, 224, 176, 255), // successSoft
                Rgba::new(37, 95, 59, 255),    // successBg
                Rgba::new(47, 122, 75, 255),   // successBorder
                Rgba::new(244, 71, 71, 255),   // danger
                Rgba::new(255, 139, 139, 255), // dangerSoft
                Rgba::new(96, 36, 40, 255),    // dangerBg
                Rgba::new(48, 24, 26, 255),    // dangerBgSubtle
                Rgba::new(141, 40, 40, 255),   // dangerSolid
                Rgba::new(152, 48, 48, 255),   // dangerBorder
                Rgba::new(255, 255, 255, 255), // onDanger
                Rgba::new(220, 220, 170, 255), // warning
                Rgba::new(215, 186, 125, 255), // warningStrong
                Rgba::new(104, 88, 48, 255),   // warningBorder
                Rgba::new(244, 71, 71, 255),   // panic
            ],
        },
    },
    ThemePreset {
        id: "midnight",
        label: "深空蓝",
        description: "默认 · 冷蓝强调",
        mode: ThemeMode::Dark,
        colors: ThemeColors {
            values: [
                Rgba::new(11, 14, 20, 255),    // bg
                Rgba::new(13, 17, 25, 255),    // bgSunken
                Rgba::new(18, 23, 34, 255),    // panel
                Rgba::new(23, 29, 43, 255),    // panelAlt
                Rgba::new(29, 32, 39, 255),    // panelRaised
                Rgba::new(24, 26, 31, 255),    // chrome
                Rgba::new(35, 43, 61, 255),    // border
                Rgba::new(51, 56, 67, 255),    // borderStrong
                Rgba::new(42, 45, 53, 255),    // borderSubtle
                Rgba::new(230, 235, 245, 255), // text
                Rgba::new(255, 255, 255, 255), // textStrong
                Rgba::new(207, 216, 234, 255), // textSoft
                Rgba::new(139, 150, 173, 255), // muted
                Rgba::new(127, 138, 158, 255), // mutedDim
                Rgba::new(23, 29, 43, 255),    // rowHover
                Rgba::new(36, 40, 50, 255),    // menuHover
                Rgba::new(42, 50, 66, 255),    // menuActive
                Rgba::new(27, 35, 51, 255),    // selection
                Rgba::new(110, 168, 255, 255), // accent
                Rgba::new(45, 104, 196, 255),  // accentSolid
                Rgba::new(255, 255, 255, 255), // onAccent
                Rgba::new(6, 18, 31, 255),     // accentInk
                Rgba::new(95, 240, 216, 255),  // accentAlt
                Rgba::new(79, 191, 135, 255),  // success
                Rgba::new(143, 224, 176, 255), // successSoft
                Rgba::new(37, 95, 59, 255),    // successBg
                Rgba::new(47, 122, 75, 255),   // successBorder
                Rgba::new(255, 107, 107, 255), // danger
                Rgba::new(255, 180, 180, 255), // dangerSoft
                Rgba::new(106, 42, 47, 255),   // dangerBg
                Rgba::new(42, 27, 31, 255),    // dangerBgSubtle
                Rgba::new(141, 48, 56, 255),   // dangerSolid
                Rgba::new(147, 64, 73, 255),   // dangerBorder
                Rgba::new(255, 228, 230, 255), // onDanger
                Rgba::new(255, 207, 107, 255), // warning
                Rgba::new(224, 168, 76, 255),  // warningStrong
                Rgba::new(122, 101, 47, 255),  // warningBorder
                Rgba::new(255, 122, 217, 255), // panic
            ],
        },
    },
    ThemePreset {
        id: "violet",
        label: "午夜紫",
        description: "紫罗兰强调 · 粉紫日志",
        mode: ThemeMode::Dark,
        colors: ThemeColors {
            values: [
                Rgba::new(11, 9, 18, 255),     // bg
                Rgba::new(13, 11, 22, 255),    // bgSunken
                Rgba::new(19, 16, 32, 255),    // panel
                Rgba::new(25, 21, 39, 255),    // panelAlt
                Rgba::new(32, 27, 48, 255),    // panelRaised
                Rgba::new(23, 19, 32, 255),    // chrome
                Rgba::new(42, 36, 64, 255),    // border
                Rgba::new(59, 51, 85, 255),    // borderStrong
                Rgba::new(36, 31, 54, 255),    // borderSubtle
                Rgba::new(238, 234, 255, 255), // text
                Rgba::new(255, 255, 255, 255), // textStrong
                Rgba::new(212, 205, 242, 255), // textSoft
                Rgba::new(154, 146, 189, 255), // muted
                Rgba::new(125, 118, 160, 255), // mutedDim
                Rgba::new(25, 21, 39, 255),    // rowHover
                Rgba::new(36, 31, 56, 255),    // menuHover
                Rgba::new(45, 39, 72, 255),    // menuActive
                Rgba::new(33, 26, 58, 255),    // selection
                Rgba::new(180, 140, 255, 255), // accent
                Rgba::new(107, 70, 201, 255),  // accentSolid
                Rgba::new(255, 255, 255, 255), // onAccent
                Rgba::new(11, 9, 18, 255),     // accentInk
                Rgba::new(255, 122, 217, 255), // accentAlt
                Rgba::new(95, 211, 154, 255),  // success
                Rgba::new(168, 240, 205, 255), // successSoft
                Rgba::new(31, 92, 64, 255),    // successBg
                Rgba::new(47, 122, 85, 255),   // successBorder
                Rgba::new(255, 122, 138, 255), // danger
                Rgba::new(255, 194, 201, 255), // dangerSoft
                Rgba::new(106, 36, 50, 255),   // dangerBg
                Rgba::new(42, 24, 32, 255),    // dangerBgSubtle
                Rgba::new(141, 47, 68, 255),   // dangerSolid
                Rgba::new(160, 58, 82, 255),   // dangerBorder
                Rgba::new(255, 230, 234, 255), // onDanger
                Rgba::new(255, 210, 122, 255), // warning
                Rgba::new(224, 168, 76, 255),  // warningStrong
                Rgba::new(122, 95, 47, 255),   // warningBorder
                Rgba::new(255, 154, 232, 255), // panic
            ],
        },
    },
    ThemePreset {
        id: "graphite",
        label: "石墨灰",
        description: "中性灰底 · 琥珀强调",
        mode: ThemeMode::Dark,
        colors: ThemeColors {
            values: [
                Rgba::new(14, 15, 17, 255),    // bg
                Rgba::new(10, 11, 13, 255),    // bgSunken
                Rgba::new(21, 23, 26, 255),    // panel
                Rgba::new(26, 28, 32, 255),    // panelAlt
                Rgba::new(33, 36, 41, 255),    // panelRaised
                Rgba::new(23, 25, 28, 255),    // chrome
                Rgba::new(42, 45, 51, 255),    // border
                Rgba::new(58, 62, 70, 255),    // borderStrong
                Rgba::new(36, 38, 43, 255),    // borderSubtle
                Rgba::new(231, 233, 236, 255), // text
                Rgba::new(255, 255, 255, 255), // textStrong
                Rgba::new(204, 208, 214, 255), // textSoft
                Rgba::new(142, 148, 158, 255), // muted
                Rgba::new(115, 121, 130, 255), // mutedDim
                Rgba::new(26, 28, 32, 255),    // rowHover
                Rgba::new(38, 41, 47, 255),    // menuHover
                Rgba::new(47, 51, 59, 255),    // menuActive
                Rgba::new(34, 38, 44, 255),    // selection
                Rgba::new(226, 180, 85, 255),  // accent
                Rgba::new(138, 106, 24, 255),  // accentSolid
                Rgba::new(255, 255, 255, 255), // onAccent
                Rgba::new(20, 18, 10, 255),    // accentInk
                Rgba::new(127, 215, 195, 255), // accentAlt
                Rgba::new(88, 189, 134, 255),  // success
                Rgba::new(158, 230, 188, 255), // successSoft
                Rgba::new(35, 85, 58, 255),    // successBg
                Rgba::new(49, 117, 79, 255),   // successBorder
                Rgba::new(255, 115, 115, 255), // danger
                Rgba::new(255, 188, 188, 255), // dangerSoft
                Rgba::new(95, 42, 42, 255),    // dangerBg
                Rgba::new(42, 27, 27, 255),    // dangerBgSubtle
                Rgba::new(141, 48, 48, 255),   // dangerSolid
                Rgba::new(160, 61, 61, 255),   // dangerBorder
                Rgba::new(255, 230, 230, 255), // onDanger
                Rgba::new(255, 208, 107, 255), // warning
                Rgba::new(224, 168, 76, 255),  // warningStrong
                Rgba::new(122, 101, 47, 255),  // warningBorder
                Rgba::new(255, 122, 217, 255), // panic
            ],
        },
    },
    ThemePreset {
        id: "forest",
        label: "森林绿",
        description: "墨绿底 · 薄荷强调",
        mode: ThemeMode::Dark,
        colors: ThemeColors {
            values: [
                Rgba::new(8, 16, 11, 255),     // bg
                Rgba::new(7, 13, 9, 255),      // bgSunken
                Rgba::new(15, 26, 19, 255),    // panel
                Rgba::new(19, 33, 23, 255),    // panelAlt
                Rgba::new(24, 40, 29, 255),    // panelRaised
                Rgba::new(14, 23, 18, 255),    // chrome
                Rgba::new(32, 56, 40, 255),    // border
                Rgba::new(45, 77, 55, 255),    // borderStrong
                Rgba::new(27, 47, 34, 255),    // borderSubtle
                Rgba::new(228, 242, 232, 255), // text
                Rgba::new(255, 255, 255, 255), // textStrong
                Rgba::new(198, 221, 205, 255), // textSoft
                Rgba::new(134, 162, 145, 255), // muted
                Rgba::new(107, 133, 119, 255), // mutedDim
                Rgba::new(19, 33, 23, 255),    // rowHover
                Rgba::new(26, 43, 32, 255),    // menuHover
                Rgba::new(33, 56, 42, 255),    // menuActive
                Rgba::new(23, 48, 31, 255),    // selection
                Rgba::new(110, 231, 160, 255), // accent
                Rgba::new(35, 122, 74, 255),   // accentSolid
                Rgba::new(255, 255, 255, 255), // onAccent
                Rgba::new(6, 18, 12, 255),     // accentInk
                Rgba::new(127, 215, 195, 255), // accentAlt
                Rgba::new(79, 191, 135, 255),  // success
                Rgba::new(169, 240, 198, 255), // successSoft
                Rgba::new(31, 92, 59, 255),    // successBg
                Rgba::new(47, 122, 82, 255),   // successBorder
                Rgba::new(255, 125, 125, 255), // danger
                Rgba::new(255, 192, 192, 255), // dangerSoft
                Rgba::new(94, 42, 44, 255),    // dangerBg
                Rgba::new(38, 26, 27, 255),    // dangerBgSubtle
                Rgba::new(141, 50, 56, 255),   // dangerSolid
                Rgba::new(160, 63, 69, 255),   // dangerBorder
                Rgba::new(255, 232, 232, 255), // onDanger
                Rgba::new(255, 210, 122, 255), // warning
                Rgba::new(224, 168, 76, 255),  // warningStrong
                Rgba::new(122, 101, 47, 255),  // warningBorder
                Rgba::new(255, 140, 224, 255), // panic
            ],
        },
    },
    ThemePreset {
        id: "paper",
        label: "纸白",
        description: "浅色底 · 高可读",
        mode: ThemeMode::Light,
        colors: ThemeColors {
            values: [
                Rgba::new(242, 244, 247, 255), // bg
                Rgba::new(232, 235, 240, 255), // bgSunken
                Rgba::new(255, 255, 255, 255), // panel
                Rgba::new(247, 248, 250, 255), // panelAlt
                Rgba::new(255, 255, 255, 255), // panelRaised
                Rgba::new(238, 241, 245, 255), // chrome
                Rgba::new(214, 218, 225, 255), // border
                Rgba::new(185, 191, 201, 255), // borderStrong
                Rgba::new(227, 230, 235, 255), // borderSubtle
                Rgba::new(32, 36, 44, 255),    // text
                Rgba::new(0, 0, 0, 255),       // textStrong
                Rgba::new(74, 82, 97, 255),    // textSoft
                Rgba::new(107, 115, 131, 255), // muted
                Rgba::new(139, 147, 161, 255), // mutedDim
                Rgba::new(238, 241, 246, 255), // rowHover
                Rgba::new(233, 237, 244, 255), // menuHover
                Rgba::new(221, 229, 242, 255), // menuActive
                Rgba::new(226, 235, 250, 255), // selection
                Rgba::new(31, 111, 235, 255),  // accent
                Rgba::new(31, 111, 235, 255),  // accentSolid
                Rgba::new(255, 255, 255, 255), // onAccent
                Rgba::new(255, 255, 255, 255), // accentInk
                Rgba::new(15, 157, 138, 255),  // accentAlt
                Rgba::new(31, 143, 85, 255),   // success
                Rgba::new(20, 107, 63, 255),   // successSoft
                Rgba::new(217, 242, 228, 255), // successBg
                Rgba::new(134, 207, 168, 255), // successBorder
                Rgba::new(208, 52, 63, 255),   // danger
                Rgba::new(163, 36, 46, 255),   // dangerSoft
                Rgba::new(251, 218, 221, 255), // dangerBg
                Rgba::new(253, 236, 237, 255), // dangerBgSubtle
                Rgba::new(208, 52, 63, 255),   // dangerSolid
                Rgba::new(231, 154, 161, 255), // dangerBorder
                Rgba::new(255, 255, 255, 255), // onDanger
                Rgba::new(176, 122, 18, 255),  // warning
                Rgba::new(138, 95, 10, 255),   // warningStrong
                Rgba::new(227, 201, 138, 255), // warningBorder
                Rgba::new(177, 63, 176, 255),  // panic
            ],
        },
    },
];

pub const DEFAULT_THEME_PRESET: &str = THEME_PRESETS[0].id;

impl ThemeColors {
    pub fn preset(id: &str) -> ThemeColors {
        THEME_PRESETS
            .iter()
            .find(|preset| preset.id == id)
            .unwrap_or(&THEME_PRESETS[0])
            .colors
    }

    /// 在预设之上叠加逐项覆盖（只存被改过的 token）。
    pub fn with_overrides(id: &str, overrides: &HashMap<ThemeToken, Rgba>) -> ThemeColors {
        let mut colors = Self::preset(id);
        for (token, color) in overrides {
            colors.set(*token, *color);
        }
        colors
    }
}

pub fn preset(id: &str) -> &'static ThemePreset {
    THEME_PRESETS
        .iter()
        .find(|preset| preset.id == id)
        .unwrap_or(&THEME_PRESETS[0])
}

/// 界面主题状态（对应 `prisma-editor.theme.v1`）。
#[derive(Debug, Clone, PartialEq)]
pub struct ThemeState {
    pub preset: String,
    pub overrides: HashMap<ThemeToken, Rgba>,
}

impl Default for ThemeState {
    fn default() -> Self {
        Self {
            preset: DEFAULT_THEME_PRESET.to_string(),
            overrides: HashMap::new(),
        }
    }
}

impl ThemeState {
    pub fn colors(&self) -> ThemeColors {
        ThemeColors::with_overrides(&self.preset, &self.overrides)
    }

    pub fn set_preset(&mut self, id: &str) {
        self.preset = id.to_string();
        self.overrides.clear();
    }

    pub fn set_override(&mut self, token: ThemeToken, color: Rgba) {
        self.overrides.insert(token, color);
    }

    pub fn reset_override(&mut self, token: ThemeToken) {
        self.overrides.remove(&token);
    }
}

/// RGB（0..1）→ HSV（色相 0..360，饱和度/明度 0..1）。
pub fn rgb_to_hsv(rgb: [f32; 3]) -> [f32; 3] {
    let [r, g, b] = rgb.map(|value| value.clamp(0.0, 1.0));
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;
    let hue = if delta < 1e-6 {
        0.0
    } else if max == r {
        60.0 * (((g - b) / delta) % 6.0)
    } else if max == g {
        60.0 * ((b - r) / delta + 2.0)
    } else {
        60.0 * ((r - g) / delta + 4.0)
    };
    let hue = if hue < 0.0 { hue + 360.0 } else { hue };
    let saturation = if max < 1e-6 { 0.0 } else { delta / max };
    [hue, saturation, max]
}

/// HSV（色相 0..360，饱和度/明度 0..1）→ RGB（0..1）。
pub fn hsv_to_rgb(hsv: [f32; 3]) -> [f32; 3] {
    let hue = hsv[0].rem_euclid(360.0);
    let saturation = hsv[1].clamp(0.0, 1.0);
    let value = hsv[2].clamp(0.0, 1.0);
    let chroma = value * saturation;
    let section = hue / 60.0;
    let x = chroma * (1.0 - (section % 2.0 - 1.0).abs());
    let (r, g, b) = match section as u32 {
        0 => (chroma, x, 0.0),
        1 => (x, chroma, 0.0),
        2 => (0.0, chroma, x),
        3 => (0.0, x, chroma),
        4 => (x, 0.0, chroma),
        _ => (chroma, 0.0, x),
    };
    let m = value - chroma;
    [r + m, g + m, b + m]
}

/// `[r, g, b, a]`（0..1）→ `#rrggbb`（`alpha` 为真时 `#rrggbbaa`）。
pub fn rgba_to_hex(rgba: [f32; 4], alpha: bool) -> String {
    let byte = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
    if alpha {
        format!(
            "#{:02x}{:02x}{:02x}{:02x}",
            byte(rgba[0]),
            byte(rgba[1]),
            byte(rgba[2]),
            byte(rgba[3])
        )
    } else {
        format!(
            "#{:02x}{:02x}{:02x}",
            byte(rgba[0]),
            byte(rgba[1]),
            byte(rgba[2])
        )
    }
}

/// `#rgb` / `#rrggbb` / `#rrggbbaa` → `[r, g, b, a]`（0..1）。
pub fn hex_to_rgba(hex: &str) -> Option<[f32; 4]> {
    let hex = hex.trim().trim_start_matches('#');
    let parse = |range: std::ops::Range<usize>| {
        u8::from_str_radix(hex.get(range)?, 16)
            .ok()
            .map(|value| f32::from(value) / 255.0)
    };
    match hex.len() {
        6 => Some([parse(0..2)?, parse(2..4)?, parse(4..6)?, 1.0]),
        8 => Some([parse(0..2)?, parse(2..4)?, parse(4..6)?, parse(6..8)?]),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hsv_and_rgb_round_trip() {
        for rgb in [
            [1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [0.0, 0.0, 1.0],
            [0.2, 0.6, 0.9],
            [0.5, 0.5, 0.5],
        ] {
            let hsv = rgb_to_hsv(rgb);
            let back = hsv_to_rgb(hsv);
            for axis in 0..3 {
                assert!(
                    (back[axis] - rgb[axis]).abs() < 1e-3,
                    "{rgb:?} → {hsv:?} → {back:?}"
                );
            }
        }
        assert_eq!(rgb_to_hsv([0.0, 0.0, 0.0])[2], 0.0);
    }

    #[test]
    fn hex_parses_and_formats_rgba() {
        assert_eq!(rgba_to_hex([1.0, 0.5, 0.0, 1.0], false), "#ff8000");
        assert_eq!(rgba_to_hex([1.0, 0.5, 0.0, 0.5], true), "#ff800080");
        let color = hex_to_rgba("#ff8000").unwrap();
        assert!((color[0] - 1.0).abs() < 1e-3);
        assert!((color[3] - 1.0).abs() < 1e-3);
        assert!((hex_to_rgba("ff800080").unwrap()[3] - 128.0 / 255.0).abs() < 1e-3);
        assert_eq!(hex_to_rgba("#xyz"), None);
        assert_eq!(hex_to_rgba("#fff"), None);
    }
}
