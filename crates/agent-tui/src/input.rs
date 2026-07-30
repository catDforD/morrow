use std::ops::Range;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Composer {
    text: String,
    cursor: usize,
    history: Vec<String>,
    history_index: Option<usize>,
    history_draft: String,
}

impl Composer {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
    }

    pub fn set(&mut self, text: impl Into<String>) {
        self.text = sanitize_input(&text.into());
        self.cursor = self.text.len();
        self.history_index = None;
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.history_index = None;
        self.history_draft.clear();
    }

    pub fn insert_char(&mut self, character: char) {
        if !character.is_control() || matches!(character, '\n' | '\t') {
            self.text.insert(self.cursor, character);
            self.cursor += character.len_utf8();
            self.leave_history();
        }
    }

    pub fn insert_str(&mut self, value: &str) {
        let value = sanitize_input(value);
        self.text.insert_str(self.cursor, &value);
        self.cursor += value.len();
        self.leave_history();
    }

    pub fn insert_newline(&mut self) {
        self.insert_char('\n');
    }

    pub fn backspace(&mut self) {
        let Some((index, _)) = self.text[..self.cursor].char_indices().next_back() else {
            return;
        };
        self.text.drain(index..self.cursor);
        self.cursor = index;
        self.leave_history();
    }

    pub fn delete(&mut self) {
        let Some(character) = self.text[self.cursor..].chars().next() else {
            return;
        };
        self.text
            .drain(self.cursor..self.cursor + character.len_utf8());
        self.leave_history();
    }

    pub fn move_left(&mut self) {
        if let Some((index, _)) = self.text[..self.cursor].char_indices().next_back() {
            self.cursor = index;
        }
    }

    pub fn move_right(&mut self) {
        if let Some(character) = self.text[self.cursor..].chars().next() {
            self.cursor += character.len_utf8();
        }
    }

    pub fn move_home(&mut self) {
        self.cursor = self.text[..self.cursor]
            .rfind('\n')
            .map_or(0, |index| index + 1);
    }

    pub fn move_end(&mut self) {
        self.cursor = self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |offset| self.cursor + offset);
    }

    pub fn move_vertical(&mut self, delta: isize) {
        let (row, column) = self.cursor_row_column();
        let target = row.saturating_add_signed(delta);
        let lines = self.text.split('\n').collect::<Vec<_>>();
        if target >= lines.len() {
            return;
        }
        let byte_column = lines[target]
            .char_indices()
            .nth(column)
            .map_or(lines[target].len(), |(index, _)| index);
        self.cursor = lines
            .iter()
            .take(target)
            .map(|line| line.len() + 1)
            .sum::<usize>()
            + byte_column;
    }

    pub fn cursor_row_column(&self) -> (usize, usize) {
        let before = &self.text[..self.cursor];
        let row = before.bytes().filter(|byte| *byte == b'\n').count();
        let line_start = before.rfind('\n').map_or(0, |index| index + 1);
        let column = before[line_start..].chars().count();
        (row, column)
    }

    pub fn replace(&mut self, range: Range<usize>, value: &str) {
        if range.start > range.end
            || range.end > self.text.len()
            || !self.text.is_char_boundary(range.start)
            || !self.text.is_char_boundary(range.end)
        {
            return;
        }
        self.text.replace_range(range.clone(), value);
        self.cursor = range.start + value.len();
        self.leave_history();
    }

    pub fn submit(&mut self) -> Option<String> {
        let prompt = self.text.trim().to_string();
        if prompt.is_empty() {
            return None;
        }
        if self.history.last() != Some(&prompt) {
            self.history.push(prompt.clone());
        }
        self.clear();
        Some(prompt)
    }

    pub fn history_previous(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let index = match self.history_index {
            None => {
                self.history_draft = self.text.clone();
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(index) => index - 1,
        };
        self.history_index = Some(index);
        self.text.clone_from(&self.history[index]);
        self.cursor = self.text.len();
    }

    pub fn history_next(&mut self) {
        let Some(index) = self.history_index else {
            return;
        };
        if index + 1 < self.history.len() {
            self.history_index = Some(index + 1);
            self.text.clone_from(&self.history[index + 1]);
        } else {
            self.history_index = None;
            self.text.clone_from(&self.history_draft);
        }
        self.cursor = self.text.len();
    }

    fn leave_history(&mut self) {
        self.history_index = None;
    }
}

pub fn sanitize_input(value: &str) -> String {
    value
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
        .collect()
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum TerminalSequence {
    #[default]
    Text,
    Escape,
    EscapeIntermediate,
    Csi,
    Osc,
    OscEscape,
    ControlString,
    ControlStringEscape,
}

/// Stateful sanitizer for output that can be split across streaming deltas.
///
/// Terminal control strings do not necessarily arrive in the same delta as their
/// terminator. Keeping the parser state prevents an incomplete escape sequence from
/// being rendered as ordinary text by the next update.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TerminalTextSanitizer {
    sequence: TerminalSequence,
}

impl TerminalTextSanitizer {
    pub(crate) fn push_to(&mut self, value: &str, output: &mut String) {
        for character in value.chars() {
            self.push_character(character, output);
        }
    }

    pub(crate) fn reset(&mut self) {
        self.sequence = TerminalSequence::Text;
    }

    fn push_character(&mut self, character: char, output: &mut String) {
        use TerminalSequence::{
            ControlString, ControlStringEscape, Csi, Escape, EscapeIntermediate, Osc, OscEscape,
            Text,
        };

        self.sequence = match self.sequence {
            Text => match character {
                '\u{1b}' => Escape,
                '\u{0090}' | '\u{0098}' | '\u{009e}' | '\u{009f}' => ControlString,
                '\u{009b}' => Csi,
                '\u{009d}' => Osc,
                character if is_safe_terminal_text(character) => {
                    output.push(character);
                    Text
                }
                _ => Text,
            },
            Escape => match character {
                '\u{1b}' => Escape,
                '[' => Csi,
                ']' => Osc,
                'P' | 'X' | '^' | '_' => ControlString,
                '\u{20}'..='\u{2f}' => EscapeIntermediate,
                '\u{30}'..='\u{7e}' => Text,
                _ => Text,
            },
            EscapeIntermediate => match character {
                '\u{1b}' => Escape,
                '\u{20}'..='\u{2f}' => EscapeIntermediate,
                '\u{30}'..='\u{7e}' => Text,
                _ => Text,
            },
            Csi => match character {
                '\u{1b}' => Escape,
                '\u{18}' | '\u{1a}' => Text,
                '\u{40}'..='\u{7e}' => Text,
                '\u{20}'..='\u{3f}' => Csi,
                _ => Csi,
            },
            Osc => match character {
                '\u{07}' | '\u{009c}' | '\u{18}' | '\u{1a}' => Text,
                '\u{1b}' => OscEscape,
                _ => Osc,
            },
            OscEscape => match character {
                '\\' | '\u{009c}' | '\u{18}' | '\u{1a}' => Text,
                '\u{1b}' => OscEscape,
                _ => Osc,
            },
            ControlString => match character {
                '\u{009c}' | '\u{18}' | '\u{1a}' => Text,
                '\u{1b}' => ControlStringEscape,
                _ => ControlString,
            },
            ControlStringEscape => match character {
                '\\' | '\u{009c}' | '\u{18}' | '\u{1a}' => Text,
                '\u{1b}' => ControlStringEscape,
                _ => ControlString,
            },
        };
    }
}

fn is_safe_terminal_text(character: char) -> bool {
    !character.is_control() || matches!(character, '\n' | '\t')
}

/// Strip terminal escape/control characters from model and tool output before rendering
/// or copying. Newlines and tabs remain intact.
pub fn sanitize_terminal_text(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    TerminalTextSanitizer::default().push_to(value, &mut output);
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composer_edits_unicode_at_char_boundaries() {
        let mut composer = Composer::default();
        composer.insert_str("你🙂好");
        composer.move_left();
        composer.backspace();
        assert_eq!(composer.text(), "你好");
        assert_eq!(composer.cursor(), "你".len());
    }

    #[test]
    fn composer_supports_multiline_history() {
        let mut composer = Composer::default();
        composer.insert_str("第一行");
        composer.insert_newline();
        composer.insert_str("第二行");
        assert_eq!(composer.submit().as_deref(), Some("第一行\n第二行"));
        composer.history_previous();
        assert_eq!(composer.text(), "第一行\n第二行");
    }

    #[test]
    fn terminal_controls_are_removed() {
        assert_eq!(
            sanitize_terminal_text("ok\u{1b}[31mred\u{1b}[0m\0\nnext"),
            "okred\nnext"
        );
    }

    #[test]
    fn terminal_control_strings_and_c1_controls_are_removed() {
        let value = concat!(
            "前🙂\u{1b}]0;secret\u{07}",
            "中\u{1b}Ppayload\u{1b}\\",
            "\u{009d}clipboard\u{009c}",
            "\u{009b}31m红\u{009b}0m",
            "\u{0098}sos\u{009c}",
            "\u{009e}pm\u{009c}",
            "\u{009f}apc\u{009c}后\n\t"
        );

        assert_eq!(sanitize_terminal_text(value), "前🙂中红后\n\t");
    }

    #[test]
    fn terminal_sequences_are_sanitized_across_chunks() {
        let mut sanitizer = TerminalTextSanitizer::default();
        let mut output = String::new();

        sanitizer.push_to("前🙂\u{1b}", &mut output);
        assert_eq!(output, "前🙂");
        sanitizer.push_to("[31", &mut output);
        assert_eq!(output, "前🙂");
        sanitizer.push_to("m红\u{1b}]0;ti", &mut output);
        assert_eq!(output, "前🙂红");
        sanitizer.push_to("tle\u{1b}", &mut output);
        assert_eq!(output, "前🙂红");
        sanitizer.push_to("\\后\n\t", &mut output);
        assert_eq!(output, "前🙂红后\n\t");
    }

    #[test]
    fn unterminated_control_strings_do_not_leak_payload() {
        assert_eq!(sanitize_terminal_text("safe\u{1b}]0;not visible"), "safe");
        assert_eq!(sanitize_terminal_text("safe\u{0090}not visible"), "safe");
    }
}
