//! 单行内联输入的编辑模型。
//!
//! 层级重命名、字段数值、搜索框、另存为路径共用同一套行为：光标、选区、
//! 按词移动、Unicode 安全。UI 只负责把按键翻译成这里的方法，再把
//! [`TextInput::text`] / [`TextInput::cursor`] 画出来。
//!
//! 光标与选区都用 **字节偏移**（始终落在 `char` 边界上），方便直接切片。

use std::ops::Range;

/// 一段可编辑的单行文本。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextInput {
    text: String,
    cursor: usize,
    anchor: usize,
}

impl TextInput {
    /// 新建并把光标放到末尾（打开输入框时的默认行为）。
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        let cursor = text.len();
        Self {
            text,
            cursor,
            anchor: cursor,
        }
    }

    /// 指定初始光标（越界或非边界时夹到最近的合法位置）。
    pub fn with_cursor(text: impl Into<String>, cursor: usize) -> Self {
        let mut input = Self::new(text);
        input.set_cursor(cursor);
        input
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// 光标所在字节偏移。
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// 光标在第几个字符（渲染/测试用）。
    pub fn cursor_char_index(&self) -> usize {
        self.text[..self.cursor].chars().count()
    }

    pub fn char_count(&self) -> usize {
        self.text.chars().count()
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// 有序选区；没有选中时为 `None`。
    pub fn selection(&self) -> Option<Range<usize>> {
        if self.cursor == self.anchor {
            None
        } else if self.cursor < self.anchor {
            Some(self.cursor..self.anchor)
        } else {
            Some(self.anchor..self.cursor)
        }
    }

    pub fn has_selection(&self) -> bool {
        self.cursor != self.anchor
    }

    pub fn selected_text(&self) -> &str {
        match self.selection() {
            Some(range) => &self.text[range],
            None => "",
        }
    }

    pub fn select_all(&mut self) {
        self.anchor = 0;
        self.cursor = self.text.len();
    }

    /// 折叠选区并移动光标（夹到边界）。
    pub fn set_cursor(&mut self, offset: usize) {
        let offset = floor_boundary(&self.text, offset.min(self.text.len()));
        self.cursor = offset;
        self.anchor = offset;
    }

    /// 替换整段文本，光标移到末尾。
    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.cursor = self.text.len();
        self.anchor = self.cursor;
    }

    pub fn clear(&mut self) {
        self.set_text("");
    }

    /// 插入文本；有选区时先删除选区。
    pub fn insert(&mut self, text: &str) {
        let text: String = text.chars().filter(|ch| !ch.is_control()).collect();
        if text.is_empty() {
            return;
        }
        self.delete_selection();
        self.text.insert_str(self.cursor, &text);
        self.cursor += text.len();
        self.anchor = self.cursor;
    }

    /// 退格：删除选区，否则删光标前一个字符。
    pub fn backspace(&mut self) {
        if self.delete_selection() {
            return;
        }
        if let Some(previous) = self.prev_boundary(self.cursor, false) {
            self.text.replace_range(previous..self.cursor, "");
            self.cursor = previous;
            self.anchor = previous;
        }
    }

    /// Delete：删除选区，否则删光标后一个字符。
    pub fn delete_forward(&mut self) {
        if self.delete_selection() {
            return;
        }
        if let Some(next) = self.next_boundary(self.cursor, false) {
            self.text.replace_range(self.cursor..next, "");
            self.anchor = self.cursor;
        }
    }

    /// Ctrl+Backspace：删光标前一个词（先吃掉空白）。
    pub fn delete_word_backward(&mut self) {
        if self.delete_selection() {
            return;
        }
        if let Some(previous) = self.prev_boundary(self.cursor, true) {
            self.text.replace_range(previous..self.cursor, "");
            self.cursor = previous;
            self.anchor = previous;
        }
    }

    /// Ctrl+Delete：删光标后一个词。
    pub fn delete_word_forward(&mut self) {
        if self.delete_selection() {
            return;
        }
        if let Some(next) = self.next_boundary(self.cursor, true) {
            self.text.replace_range(self.cursor..next, "");
            self.anchor = self.cursor;
        }
    }

    pub fn move_left(&mut self, extend: bool, word: bool) {
        if !extend {
            if let Some(range) = self.selection() {
                self.cursor = range.start;
                self.anchor = range.start;
                return;
            }
        }
        if let Some(previous) = self.prev_boundary(self.cursor, word) {
            self.move_to(previous, extend);
        } else {
            self.move_to(0, extend);
        }
    }

    pub fn move_right(&mut self, extend: bool, word: bool) {
        if !extend {
            if let Some(range) = self.selection() {
                self.cursor = range.end;
                self.anchor = range.end;
                return;
            }
        }
        if let Some(next) = self.next_boundary(self.cursor, word) {
            self.move_to(next, extend);
        } else {
            self.move_to(self.text.len(), extend);
        }
    }

    pub fn move_home(&mut self, extend: bool) {
        self.move_to(0, extend);
    }

    pub fn move_end(&mut self, extend: bool) {
        self.move_to(self.text.len(), extend);
    }

    fn move_to(&mut self, offset: usize, extend: bool) {
        self.cursor = floor_boundary(&self.text, offset.min(self.text.len()));
        if !extend {
            self.anchor = self.cursor;
        }
    }

    /// 删除选区，返回是否删了东西。
    fn delete_selection(&mut self) -> bool {
        let Some(range) = self.selection() else {
            return false;
        };
        self.text.replace_range(range.clone(), "");
        self.cursor = range.start;
        self.anchor = range.start;
        true
    }

    fn prev_boundary(&self, offset: usize, word: bool) -> Option<usize> {
        let prefix = &self.text[..offset];
        let mut chars = prefix.char_indices().rev();
        if !word {
            return chars.next().map(|(index, _)| index);
        }
        // 光标前是空白/标点时先跳过它们，再跳过整词；否则只跳当前词。
        let mut result = offset;
        let mut at_word = false;
        if let Some((_, ch)) = chars.clone().next() {
            at_word = is_word_char(ch);
        }
        if !at_word {
            for (index, ch) in chars.by_ref() {
                if is_word_char(ch) {
                    break;
                }
                result = index;
            }
        }
        for (index, ch) in chars {
            if !is_word_char(ch) {
                break;
            }
            result = index;
        }
        if result == offset {
            None
        } else {
            Some(result)
        }
    }

    fn next_boundary(&self, offset: usize, word: bool) -> Option<usize> {
        let suffix = &self.text[offset..];
        let mut chars = suffix.char_indices();
        if !word {
            return chars
                .next()
                .map(|(index, ch)| offset + index + ch.len_utf8());
        }
        // 先走完当前词，再吃掉后面的空白/标点，停在下个词首。
        let mut result = offset;
        let mut in_word = suffix.chars().next().map(is_word_char).unwrap_or(false);
        for (index, ch) in chars {
            let word_char = is_word_char(ch);
            if in_word {
                result = offset + index + ch.len_utf8();
                if !word_char {
                    in_word = false;
                }
            } else if word_char {
                break;
            } else {
                result = offset + index + ch.len_utf8();
            }
        }
        if result == offset {
            None
        } else {
            Some(result)
        }
    }
}

fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

/// 把任意字节偏移夹到最近的前一个 `char` 边界。
fn floor_boundary(text: &str, mut offset: usize) -> usize {
    offset = offset.min(text.len());
    while offset > 0 && !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_puts_cursor_at_end() {
        let input = TextInput::new("Player");
        assert_eq!(input.cursor(), 6);
        assert_eq!(input.selected_text(), "");
        assert!(!input.has_selection());
    }

    #[test]
    fn insert_replaces_selection() {
        let mut input = TextInput::new("Player");
        input.set_cursor(0);
        input.select_all();
        input.insert("Enemy");
        assert_eq!(input.text(), "Enemy");
        assert_eq!(input.cursor(), 5);
        assert!(!input.has_selection());
    }

    #[test]
    fn insert_ignores_control_chars() {
        let mut input = TextInput::new("");
        input.insert("a\nb\tc");
        assert_eq!(input.text(), "abc");
    }

    #[test]
    fn backspace_and_delete_are_unicode_safe() {
        let mut input = TextInput::new("aé中");
        assert_eq!(input.cursor(), 6);
        input.backspace();
        assert_eq!(input.text(), "aé");
        input.set_cursor(1);
        input.delete_forward();
        assert_eq!(input.text(), "a");
    }

    #[test]
    fn backspace_with_selection_deletes_only_selection() {
        let mut input = TextInput::new("abcdef");
        input.set_cursor(2);
        input.move_right(true, false);
        input.move_right(true, false);
        assert_eq!(input.selected_text(), "cd");
        input.backspace();
        assert_eq!(input.text(), "abef");
        assert_eq!(input.cursor(), 2);
    }

    #[test]
    fn move_left_collapses_selection_to_start() {
        let mut input = TextInput::new("abcdef");
        input.set_cursor(1);
        input.move_right(true, false);
        input.move_right(true, false);
        input.move_left(false, false);
        assert_eq!(input.cursor(), 1);
        assert!(!input.has_selection());
    }

    #[test]
    fn move_right_collapses_selection_to_end() {
        let mut input = TextInput::new("abcdef");
        input.set_cursor(1);
        input.move_right(true, false);
        input.move_right(true, false);
        input.move_right(false, false);
        assert_eq!(input.cursor(), 3);
        assert!(!input.has_selection());
    }

    #[test]
    fn word_moves_skip_whitespace_and_word() {
        let mut input = TextInput::new("hello brave world");
        input.move_left(false, true);
        assert_eq!(input.cursor(), 12);
        input.move_left(false, true);
        assert_eq!(input.cursor(), 6);
        input.move_right(false, true);
        assert_eq!(input.cursor(), 12);
    }

    #[test]
    fn word_delete_eats_previous_word() {
        let mut input = TextInput::new("hello world  ");
        input.delete_word_backward();
        assert_eq!(input.text(), "hello ");
        input.delete_word_backward();
        assert_eq!(input.text(), "");
    }

    #[test]
    fn word_delete_forward_eats_next_word() {
        let mut input = TextInput::new("  hello world");
        input.set_cursor(0);
        input.delete_word_forward();
        assert_eq!(input.text(), "hello world");
    }

    #[test]
    fn home_end_and_extend() {
        let mut input = TextInput::new("abcdef");
        input.move_home(true);
        assert_eq!(input.selected_text(), "abcdef");
        input.move_end(false);
        assert_eq!(input.cursor(), 6);
        assert!(!input.has_selection());
    }

    #[test]
    fn set_cursor_clamps_to_char_boundary() {
        let mut input = TextInput::new("é");
        input.set_cursor(1);
        assert_eq!(input.cursor(), 0);
        input.set_cursor(99);
        assert_eq!(input.cursor(), 2);
    }

    #[test]
    fn selected_text_orders_backwards_selection() {
        let mut input = TextInput::new("abcdef");
        input.select_all();
        input.move_left(true, false);
        input.move_left(true, false);
        assert_eq!(input.selection(), Some(0..4));
        assert_eq!(input.selected_text(), "abcd");
    }

    #[test]
    fn with_cursor_keeps_given_position() {
        let input = TextInput::with_cursor("abc", 1);
        assert_eq!(input.cursor(), 1);
        assert_eq!(input.text(), "abc");
    }

    #[test]
    fn no_op_moves_stay_put() {
        let mut input = TextInput::new("");
        input.move_left(false, true);
        input.move_right(false, false);
        input.backspace();
        input.delete_forward();
        assert_eq!(input.text(), "");
        assert_eq!(input.cursor(), 0);
    }
}
