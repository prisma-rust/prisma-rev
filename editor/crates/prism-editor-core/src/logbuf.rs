//! 控制台日志缓冲（`EditorLogger` / `preview-log` 的等价物）。
//!
//! 面板本身是窗口化的：只渲染视口附近的行，其余用 spacer 占位。
//! 这里提供数据侧需要的全部操作：环形缓冲、级别过滤、来源过滤、搜索、
//! 以及「给我视口附近的 index 区间」。

use std::collections::VecDeque;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
    Panic,
}

impl LogLevel {
    pub fn label(self) -> &'static str {
        match self {
            LogLevel::Error => "error",
            LogLevel::Warn => "warn",
            LogLevel::Info => "info",
            LogLevel::Debug => "debug",
            LogLevel::Trace => "trace",
            LogLevel::Panic => "panic",
        }
    }

    pub fn parse(text: &str) -> LogLevel {
        let lower = text.to_ascii_lowercase();
        if lower.contains("panic") {
            return LogLevel::Panic;
        }
        if lower.contains("error") {
            return LogLevel::Error;
        }
        if lower.contains("warn") {
            return LogLevel::Warn;
        }
        if lower.contains("debug") {
            return LogLevel::Debug;
        }
        if lower.contains("trace") {
            return LogLevel::Trace;
        }
        LogLevel::Info
    }

    /// 从 `env_logger` 风格的一行里推断级别（`[WARN] ...` / `WARN ...`）。
    pub fn from_line(line: &str) -> LogLevel {
        let head: String = line.chars().take(24).collect();
        LogLevel::parse(&head)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LogSource {
    /// 编辑器自身（UI / 命令 / 文档）。
    Editor,
    /// 引擎（进程内视口线程里的 `prism_render` / `prism_engine`）。
    Engine,
    /// 游戏进程的 stdout/stderr。
    Game,
    /// 编辑器产生的过程记录（构建、cook）。
    System,
}

impl LogSource {
    pub fn label(self) -> &'static str {
        match self {
            LogSource::Editor => "editor",
            LogSource::Engine => "engine",
            LogSource::Game => "game",
            LogSource::System => "system",
        }
    }

    pub fn all() -> [LogSource; 4] {
        [
            LogSource::Editor,
            LogSource::Engine,
            LogSource::Game,
            LogSource::System,
        ]
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LogLine {
    pub source: LogSource,
    pub level: LogLevel,
    pub text: String,
    /// 单调递增的序号（用于 `uniform_list` 的稳定 id）。
    pub seq: u64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LogFilter {
    /// 选中的级别；空 = 全部。
    pub levels: Vec<LogLevel>,
    /// 选中的来源；空 = 全部。
    pub sources: Vec<LogSource>,
    pub search: String,
}

impl LogFilter {
    pub fn accepts(&self, line: &LogLine) -> bool {
        if !self.levels.is_empty() && !self.levels.contains(&line.level) {
            return false;
        }
        if !self.sources.is_empty() && !self.sources.contains(&line.source) {
            return false;
        }
        if !self.search.is_empty()
            && !line
                .text
                .to_ascii_lowercase()
                .contains(&self.search.to_ascii_lowercase())
        {
            return false;
        }
        true
    }

    pub fn toggle_level(&mut self, level: LogLevel) {
        toggle(&mut self.levels, level);
    }

    pub fn toggle_source(&mut self, source: LogSource) {
        toggle(&mut self.sources, source);
    }

    pub fn is_empty(&self) -> bool {
        self.levels.is_empty() && self.sources.is_empty() && self.search.is_empty()
    }
}

fn toggle<T: PartialEq>(values: &mut Vec<T>, value: T) {
    match values.iter().position(|item| *item == value) {
        Some(index) => {
            values.remove(index);
        }
        None => values.push(value),
    }
}

#[derive(Debug, Clone)]
pub struct LogBuffer {
    lines: VecDeque<LogLine>,
    capacity: usize,
    filter: LogFilter,
    next_seq: u64,
    /// 每次追加/清空自增，供 UI 判断是否需要重新计算过滤结果。
    revision: u64,
    /// 过滤结果的缓存（`revision` 变化时重建）。
    visible_cache: Vec<usize>,
    cache_revision: u64,
}

pub const DEFAULT_LOG_CAPACITY: usize = 5000;
pub const MAX_LOG_CAPACITY: usize = 20_000;

impl Default for LogBuffer {
    fn default() -> Self {
        Self::new(DEFAULT_LOG_CAPACITY)
    }
}

impl LogBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            capacity: capacity.clamp(1, MAX_LOG_CAPACITY),
            filter: LogFilter::default(),
            next_seq: 1,
            revision: 1,
            visible_cache: Vec::new(),
            cache_revision: 0,
        }
    }

    pub fn set_capacity(&mut self, capacity: usize) {
        self.capacity = capacity.clamp(1, MAX_LOG_CAPACITY);
        while self.lines.len() > self.capacity {
            self.lines.pop_front();
        }
        self.revision += 1;
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn filter(&self) -> &LogFilter {
        &self.filter
    }

    pub fn filter_mut(&mut self) -> &mut LogFilter {
        self.revision += 1;
        &mut self.filter
    }

    pub fn set_filter(&mut self, filter: LogFilter) {
        self.filter = filter;
        self.revision += 1;
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn push(&mut self, source: LogSource, level: LogLevel, text: impl Into<String>) {
        let line = LogLine {
            source,
            level,
            text: text.into(),
            seq: self.next_seq,
        };
        self.next_seq += 1;
        self.lines.push_back(line);
        while self.lines.len() > self.capacity {
            self.lines.pop_front();
        }
        self.revision += 1;
    }

    /// 追加一行「已经是文本」的日志，级别从文本前缀推断。
    pub fn push_line(&mut self, source: LogSource, text: impl Into<String>) {
        let text = text.into();
        let level = LogLevel::from_line(&text);
        self.push(source, level, text);
    }

    pub fn clear(&mut self) {
        self.lines.clear();
        self.revision += 1;
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    pub fn get(&self, index: usize) -> Option<&LogLine> {
        self.lines.get(index)
    }

    pub fn all(&self) -> &VecDeque<LogLine> {
        &self.lines
    }

    /// 过滤后的行数（面板滚动区高度）。
    pub fn visible_len(&mut self) -> usize {
        self.ensure_cache();
        self.visible_cache.len()
    }

    pub fn visible_line(&mut self, index: usize) -> Option<&LogLine> {
        self.ensure_cache();
        self.visible_cache
            .get(index)
            .and_then(|raw| self.lines.get(*raw))
    }

    /// 视口窗口（前端的 `windowRange`）：总行数、起始行、结束行。
    /// 当前过滤结果的一份快照（`uniform_list` 的 processor 不能借用宿主状态）。
    pub fn visible_lines(&mut self) -> Vec<LogLine> {
        self.ensure_cache();
        self.visible_cache
            .iter()
            .filter_map(|index| self.lines.get(*index).cloned())
            .collect()
    }

    pub fn window(&mut self, scroll_top: f32, row_height: f32, viewport_height: f32) -> LogWindow {
        let total = self.visible_len();
        let first = if row_height <= 0.0 {
            0
        } else {
            (scroll_top / row_height).floor().max(0.0) as usize
        };
        let rows = if row_height <= 0.0 {
            total
        } else {
            (viewport_height / row_height).ceil() as usize + 1
        };
        let first = first.min(total);
        let last = (first + rows).min(total);
        LogWindow {
            total,
            first,
            last,
            top_spacer: first as f32 * row_height,
            bottom_spacer: (total.saturating_sub(last)) as f32 * row_height,
        }
    }

    pub fn error_count(&self) -> usize {
        self.lines
            .iter()
            .filter(|line| matches!(line.level, LogLevel::Error | LogLevel::Panic))
            .count()
    }

    pub fn warn_count(&self) -> usize {
        self.lines
            .iter()
            .filter(|line| line.level == LogLevel::Warn)
            .count()
    }

    fn ensure_cache(&mut self) {
        if self.cache_revision == self.revision {
            return;
        }
        self.visible_cache = self
            .lines
            .iter()
            .enumerate()
            .filter(|(_, line)| self.filter.accepts(line))
            .map(|(index, _)| index)
            .collect();
        self.cache_revision = self.revision;
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LogWindow {
    pub total: usize,
    pub first: usize,
    pub last: usize,
    /// 上方 spacer 高度（像素）。
    pub top_spacer: f32,
    /// 下方 spacer 高度（像素）。
    pub bottom_spacer: f32,
}

pub const LOG_ROW_HEIGHT: f32 = 18.0;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_buffer_drops_oldest_lines() {
        let mut buffer = LogBuffer::new(3);
        for index in 0..5 {
            buffer.push(LogSource::Editor, LogLevel::Info, format!("line {index}"));
        }
        assert_eq!(buffer.len(), 3);
        assert_eq!(buffer.get(0).unwrap().text, "line 2");
        assert_eq!(buffer.get(2).unwrap().text, "line 4");
    }

    #[test]
    fn capacity_is_clamped() {
        let buffer = LogBuffer::new(usize::MAX);
        assert_eq!(buffer.capacity(), MAX_LOG_CAPACITY);
        let buffer = LogBuffer::new(0);
        assert_eq!(buffer.capacity(), 1);
    }

    #[test]
    fn filters_by_level_source_and_search() {
        let mut buffer = LogBuffer::default();
        buffer.push(LogSource::Engine, LogLevel::Warn, "shadow map missing");
        buffer.push(LogSource::Game, LogLevel::Error, "panic in update");
        buffer.push(LogSource::Editor, LogLevel::Info, "saved scene");

        assert_eq!(buffer.visible_len(), 3);

        buffer.filter_mut().toggle_level(LogLevel::Info);
        assert_eq!(buffer.visible_len(), 1, "只留 Info");

        buffer.filter_mut().levels.clear();
        buffer.filter_mut().toggle_source(LogSource::Engine);
        assert_eq!(buffer.visible_len(), 1);
        assert_eq!(buffer.visible_line(0).unwrap().text, "shadow map missing");

        buffer.filter_mut().sources.clear();
        buffer.filter_mut().search = "PANIC".into();
        assert_eq!(buffer.visible_len(), 1, "搜索不区分大小写");
    }

    #[test]
    fn cache_invalidates_on_new_lines() {
        let mut buffer = LogBuffer::default();
        buffer.push(LogSource::Editor, LogLevel::Info, "a");
        assert_eq!(buffer.visible_len(), 1);
        buffer.push(LogSource::Editor, LogLevel::Info, "b");
        assert_eq!(buffer.visible_len(), 2);

        buffer.filter_mut().search = "b".into();
        assert_eq!(buffer.visible_len(), 1);
        buffer.clear();
        assert_eq!(buffer.visible_len(), 0);
    }

    #[test]
    fn window_matches_fixed_row_height() {
        let mut buffer = LogBuffer::default();
        for index in 0..100 {
            buffer.push(LogSource::Editor, LogLevel::Info, format!("line {index}"));
        }
        let window = buffer.window(9.0 * LOG_ROW_HEIGHT, LOG_ROW_HEIGHT, 180.0);
        assert_eq!(window.total, 100);
        assert_eq!(window.first, 9);
        assert_eq!(window.last, 20, "180/18 = 10 行 + 1");
        assert_eq!(window.top_spacer, 9.0 * LOG_ROW_HEIGHT);
        assert_eq!(window.bottom_spacer, 80.0 * LOG_ROW_HEIGHT);

        // 滚过底部的越界请求被夹住。
        let window = buffer.window(1_000.0 * LOG_ROW_HEIGHT, LOG_ROW_HEIGHT, 18.0);
        assert_eq!(window.first, 100);
        assert_eq!(window.last, 100);
        assert_eq!(window.bottom_spacer, 0.0);
    }

    #[test]
    fn counts_errors_and_warnings() {
        let mut buffer = LogBuffer::default();
        buffer.push(LogSource::Editor, LogLevel::Error, "e1");
        buffer.push(LogSource::Editor, LogLevel::Panic, "p1");
        buffer.push(LogSource::Editor, LogLevel::Warn, "w1");
        assert_eq!(buffer.error_count(), 2);
        assert_eq!(buffer.warn_count(), 1);
    }

    #[test]
    fn level_inferred_from_line_prefix() {
        assert_eq!(LogLevel::from_line("[ERROR] boom"), LogLevel::Error);
        assert_eq!(LogLevel::from_line("WARN  slow"), LogLevel::Warn);
        assert_eq!(LogLevel::from_line("[INFO] ok"), LogLevel::Info);
        assert_eq!(LogLevel::from_line("thread panicked"), LogLevel::Panic);
        assert_eq!(LogLevel::from_line("plain output"), LogLevel::Info);
    }

    #[test]
    fn clear_resets_visible_but_keeps_sequence_increasing() {
        let mut buffer = LogBuffer::default();
        buffer.push(LogSource::Editor, LogLevel::Info, "a");
        buffer.clear();
        buffer.push(LogSource::Editor, LogLevel::Info, "b");
        assert_eq!(buffer.len(), 1);
        assert!(buffer.get(0).unwrap().seq > 1);
    }
}
