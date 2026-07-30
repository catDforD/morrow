use std::io::{self, Stdout, Write};

#[cfg(unix)]
use crossterm::event::{
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::{
    Command,
    cursor::MoveTo,
    event::{DisableBracketedPaste, EnableBracketedPaste},
    execute,
    style::Print,
    terminal::{
        BeginSynchronizedUpdate, EndSynchronizedUpdate, disable_raw_mode, enable_raw_mode, size,
    },
};
use ratatui::{
    Frame, Terminal, TerminalOptions, Viewport,
    backend::CrosstermBackend,
    buffer::Buffer,
    layout::{Rect, Size},
};

#[derive(Debug, Clone, Default)]
pub(crate) struct InlineRender {
    pub scrollback: Option<ScrollbackFrame>,
}

#[derive(Debug, Clone)]
pub(crate) struct ScrollbackFrame {
    pub session_id: Option<String>,
    pub x: u16,
    pub rows: Buffer,
}

#[derive(Debug, Clone)]
struct ScrollbackState {
    session_id: Option<String>,
    terminal_width: u16,
    x: u16,
    rows: Buffer,
}

#[derive(Debug, Clone)]
struct ScrollbackAppend {
    x: u16,
    rows: Buffer,
}

#[derive(Debug, Default)]
struct ScrollbackTracker {
    state: Option<ScrollbackState>,
}

impl ScrollbackTracker {
    fn update(
        &mut self,
        frame: Option<ScrollbackFrame>,
        terminal_width: u16,
    ) -> Option<ScrollbackAppend> {
        let frame = frame?;
        let next = ScrollbackState {
            session_id: frame.session_id,
            terminal_width,
            x: frame.x,
            rows: frame.rows,
        };
        let Some(previous) = self.state.as_ref() else {
            let append = buffer_rows(&next.rows, 0, next.rows.area.height);
            let x = next.x;
            self.state = Some(next);
            return append.map(|rows| ScrollbackAppend { x, rows });
        };

        let same_layout = previous.session_id == next.session_id
            && previous.terminal_width == next.terminal_width
            && previous.x == next.x
            && previous.rows.area.width == next.rows.area.width;
        if !same_layout {
            let changed_session = previous.session_id != next.session_id;
            let append = changed_session
                .then(|| buffer_rows(&next.rows, 0, next.rows.area.height))
                .flatten()
                .map(|rows| ScrollbackAppend { x: next.x, rows });
            self.state = Some(next);
            return append;
        }

        let common_rows = common_prefix_rows(&previous.rows, &next.rows);
        if common_rows < previous.rows.area.height.min(next.rows.area.height) {
            self.state = Some(next);
            return None;
        }
        if next.rows.area.height <= previous.rows.area.height {
            return None;
        }

        let append = buffer_rows(&next.rows, previous.rows.area.height, next.rows.area.height)
            .map(|rows| ScrollbackAppend { x: next.x, rows });
        self.state = Some(next);
        append
    }
}

fn common_prefix_rows(left: &Buffer, right: &Buffer) -> u16 {
    if left.area.width != right.area.width {
        return 0;
    }
    let width = usize::from(left.area.width);
    let rows = left.area.height.min(right.area.height);
    (0..rows)
        .take_while(|row| {
            let start = usize::from(*row) * width;
            let end = start + width;
            left.content[start..end] == right.content[start..end]
        })
        .count() as u16
}

fn buffer_rows(buffer: &Buffer, start: u16, end: u16) -> Option<Buffer> {
    let end = end.min(buffer.area.height);
    if start >= end {
        return None;
    }
    let width = usize::from(buffer.area.width);
    let content_start = usize::from(start) * width;
    let content_end = usize::from(end) * width;
    Some(Buffer {
        area: Rect::new(0, 0, buffer.area.width, end - start),
        content: buffer.content[content_start..content_end].to_vec(),
    })
}

/// A Ratatui terminal embedded in the normal terminal screen.
///
/// Ratatui owns the buffer diff and scrolling-region implementation. Morrow adds synchronized
/// updates and makes sure an interrupted render can never leave the terminal in update mode.
pub struct InlineTerminal {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    synchronized_update_enabled: bool,
    scrolling_region_maybe_set: bool,
    previous_frame: Option<Buffer>,
    viewport_area: Option<Rect>,
    scrollback: ScrollbackTracker,
    inline_height: u16,
}

pub type MorrowTerminal = InlineTerminal;

impl InlineTerminal {
    fn new(stdout: Stdout) -> io::Result<Self> {
        let (_, height) = size()?;
        let terminal = Terminal::with_options(
            CrosstermBackend::new(stdout),
            TerminalOptions {
                // Passing u16::MAX here would make Ratatui append roughly 65k lines while
                // reserving the viewport. Use the actual terminal height instead.
                viewport: Viewport::Inline(height.max(1)),
            },
        )?;
        Ok(Self {
            terminal,
            synchronized_update_enabled: false,
            scrolling_region_maybe_set: false,
            previous_frame: None,
            viewport_area: None,
            scrollback: ScrollbackTracker::default(),
            inline_height: height.max(1),
        })
    }

    pub fn size(&self) -> io::Result<Size> {
        self.terminal.size()
    }

    pub fn draw<F>(&mut self, render: F) -> io::Result<()>
    where
        F: FnOnce(&mut Frame<'_>),
    {
        self.draw_inline(|frame| {
            render(frame);
            InlineRender::default()
        })
    }

    pub(crate) fn draw_inline<F>(&mut self, render: F) -> io::Result<()>
    where
        F: FnOnce(&mut Frame<'_>) -> InlineRender,
    {
        self.ensure_full_height()?;
        self.synchronized_update_enabled = true;
        execute!(self.terminal.backend_mut(), BeginSynchronizedUpdate)?;
        let mut inline_render = None;
        let draw_result = self.terminal.draw(|frame| {
            inline_render = Some(render(frame));
        });
        let operation_result = match draw_result {
            Ok(completed) => {
                let terminal_width = completed.area.width;
                self.previous_frame = Some(completed.buffer.clone());
                self.viewport_area = Some(completed.area);
                let append = self.scrollback.update(
                    inline_render.and_then(|render| render.scrollback),
                    terminal_width,
                );
                if let Some(append) = append {
                    self.insert_scrollback(append)
                } else {
                    Ok(())
                }
            }
            Err(error) => Err(error),
        };
        let end_result = self.end_synchronized_update();
        operation_result.and(end_result)
    }

    fn ensure_full_height(&mut self) -> io::Result<()> {
        let size = self.terminal.size()?;
        if size.height <= self.inline_height {
            return Ok(());
        }

        let anchor = self.viewport_area.map_or(0, |area| area.y);
        execute!(self.terminal.backend_mut(), MoveTo(0, anchor))?;
        let replacement = Terminal::with_options(
            CrosstermBackend::new(io::stdout()),
            TerminalOptions {
                viewport: Viewport::Inline(size.height),
            },
        )?;
        self.terminal = replacement;
        self.inline_height = size.height;
        self.previous_frame = None;
        self.viewport_area = None;
        Ok(())
    }

    pub(crate) fn finish<F>(&mut self, render: F) -> io::Result<()>
    where
        F: FnOnce(&mut Frame<'_>) -> InlineRender,
    {
        self.draw_inline(render)?;
        let size = self.size()?;
        execute!(
            self.terminal.backend_mut(),
            MoveTo(0, size.height.saturating_sub(1)),
            Print("\r\n")
        )?;
        Ok(())
    }

    fn insert_scrollback(&mut self, append: ScrollbackAppend) -> io::Result<()> {
        let height = append.rows.area.height;
        if height == 0 {
            return Ok(());
        }
        self.scrolling_region_maybe_set = true;
        let x_offset = append.x;
        let source = append.rows;
        let result = self.terminal.insert_before(height, |target| {
            let width = source
                .area
                .width
                .min(target.area.width.saturating_sub(x_offset));
            for y in 0..source.area.height.min(target.area.height) {
                for x in 0..width {
                    if let (Some(source_cell), Some(target_cell)) =
                        (source.cell((x, y)), target.cell_mut((x_offset + x, y)))
                    {
                        *target_cell = source_cell.clone();
                    }
                }
            }
        });
        if result.is_ok() {
            self.scrolling_region_maybe_set = false;
        }
        result
    }

    fn end_synchronized_update(&mut self) -> io::Result<()> {
        if !self.synchronized_update_enabled {
            return Ok(());
        }
        execute!(self.terminal.backend_mut(), EndSynchronizedUpdate)?;
        self.synchronized_update_enabled = false;
        Ok(())
    }

    fn restore_output(&mut self) -> io::Result<()> {
        let mut first_error = self.end_synchronized_update().err();
        if self.scrolling_region_maybe_set {
            match execute!(self.terminal.backend_mut(), Print("\x1b[r")) {
                Ok(()) => self.scrolling_region_maybe_set = false,
                Err(error) if first_error.is_none() => first_error = Some(error),
                Err(_) => {}
            }
        }
        if let Err(error) = self.terminal.show_cursor()
            && first_error.is_none()
        {
            first_error = Some(error);
        }
        first_error.map_or(Ok(()), Err)
    }

    pub fn backend_mut(&mut self) -> &mut CrosstermBackend<Stdout> {
        self.terminal.backend_mut()
    }
}

/// Owns every terminal mode enabled by Morrow and restores it on all return and panic paths.
pub struct TerminalGuard {
    terminal: Option<MorrowTerminal>,
    modes: TerminalModes,
}

#[derive(Debug, Default)]
struct TerminalModes {
    raw_mode_enabled: bool,
    bracketed_paste_enabled: bool,
    #[cfg(unix)]
    keyboard_enhancement_enabled: bool,
}

impl TerminalModes {
    fn restore_output_modes<W: Write>(&mut self, writer: &mut W) -> io::Result<()> {
        let mut first_error = None;
        #[cfg(unix)]
        restore_output_mode(
            writer,
            &mut self.keyboard_enhancement_enabled,
            PopKeyboardEnhancementFlags,
            &mut first_error,
        );
        restore_output_mode(
            writer,
            &mut self.bracketed_paste_enabled,
            DisableBracketedPaste,
            &mut first_error,
        );
        first_error.map_or(Ok(()), Err)
    }

    fn output_modes_restored(&self) -> bool {
        !self.bracketed_paste_enabled && {
            #[cfg(unix)]
            {
                !self.keyboard_enhancement_enabled
            }
            #[cfg(not(unix))]
            {
                true
            }
        }
    }
}

fn enable_output_mode<W, C>(writer: &mut W, enabled: &mut bool, command: C) -> io::Result<()>
where
    W: Write,
    C: Command,
{
    *enabled = true;
    execute!(writer, command)?;
    Ok(())
}

fn restore_output_mode<W, C>(
    writer: &mut W,
    enabled: &mut bool,
    command: C,
    first_error: &mut Option<io::Error>,
) where
    W: Write,
    C: Command,
{
    if !*enabled {
        return;
    }
    match execute!(writer, command) {
        Ok(()) => *enabled = false,
        Err(error) if first_error.is_none() => *first_error = Some(error),
        Err(_) => {}
    }
}

fn restore_failed_setup<W: Write>(modes: &mut TerminalModes, writer: &mut W) {
    for _ in 0..2 {
        if modes.output_modes_restored() {
            break;
        }
        let _ = modes.restore_output_modes(writer);
    }
    for _ in 0..2 {
        if !modes.raw_mode_enabled {
            break;
        }
        if disable_raw_mode().is_ok() {
            modes.raw_mode_enabled = false;
        }
    }
}

impl TerminalGuard {
    pub fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut modes = TerminalModes {
            raw_mode_enabled: true,
            ..TerminalModes::default()
        };
        let mut stdout = io::stdout();
        let setup = (|| {
            enable_output_mode(
                &mut stdout,
                &mut modes.bracketed_paste_enabled,
                EnableBracketedPaste,
            )?;
            #[cfg(unix)]
            enable_output_mode(
                &mut stdout,
                &mut modes.keyboard_enhancement_enabled,
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
            )?;
            Ok::<(), io::Error>(())
        })();
        if let Err(error) = setup {
            restore_failed_setup(&mut modes, &mut stdout);
            return Err(error);
        }
        let terminal = match InlineTerminal::new(stdout) {
            Ok(terminal) => terminal,
            Err(error) => {
                let mut stdout = io::stdout();
                restore_failed_setup(&mut modes, &mut stdout);
                return Err(error);
            }
        };
        Ok(Self {
            terminal: Some(terminal),
            modes,
        })
    }

    pub fn terminal_mut(&mut self) -> &mut MorrowTerminal {
        self.terminal
            .as_mut()
            .expect("terminal is unavailable after restoration")
    }

    pub fn restore(&mut self) -> io::Result<()> {
        let mut first_error = None;
        let mut terminal_output_restored = self.terminal.is_none();
        if let Some(terminal) = self.terminal.as_mut() {
            match terminal.restore_output() {
                Ok(()) => terminal_output_restored = true,
                Err(error) => first_error = Some(error),
            }
            if let Err(error) = self.modes.restore_output_modes(terminal.backend_mut())
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        if self.modes.raw_mode_enabled {
            match disable_raw_mode() {
                Ok(()) => self.modes.raw_mode_enabled = false,
                Err(error) if first_error.is_none() => first_error = Some(error),
                Err(_) => {}
            }
        }
        if terminal_output_restored && self.modes.output_modes_restored() {
            self.terminal.take();
        }
        first_error.map_or(Ok(()), Err)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_rows(width: u16, lines: &[&str]) -> Buffer {
        let mut buffer = Buffer::empty(Rect::new(0, 0, width, lines.len() as u16));
        for (y, line) in lines.iter().enumerate() {
            buffer.set_string(0, y as u16, *line, ratatui::style::Style::default());
        }
        buffer
    }

    fn scrollback_frame(session_id: &str, width: u16, lines: &[&str]) -> ScrollbackFrame {
        ScrollbackFrame {
            session_id: Some(session_id.to_string()),
            x: 1,
            rows: test_rows(width, lines),
        }
    }

    fn buffer_lines(buffer: &Buffer) -> Vec<String> {
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .filter_map(|x| buffer.cell((x, y)))
                    .map(|cell| cell.symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[derive(Default)]
    struct FailFirstWrite {
        failed: bool,
        output: Vec<u8>,
    }

    impl Write for FailFirstWrite {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            if !self.failed {
                self.failed = true;
                return Err(io::Error::other("injected write failure"));
            }
            self.output.extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn restore_continues_after_an_output_mode_fails_and_retries_only_that_mode() {
        let mut modes = TerminalModes {
            bracketed_paste_enabled: true,
            #[cfg(unix)]
            keyboard_enhancement_enabled: true,
            raw_mode_enabled: false,
        };
        let mut writer = FailFirstWrite::default();

        assert!(modes.restore_output_modes(&mut writer).is_err());
        assert!(!modes.bracketed_paste_enabled);
        #[cfg(unix)]
        assert!(modes.keyboard_enhancement_enabled);

        modes.restore_output_modes(&mut writer).unwrap();
        assert!(modes.output_modes_restored());
        #[cfg(unix)]
        assert!(writer.output.ends_with(b"\x1b[<1u"));
    }

    #[test]
    fn failed_mode_enable_is_still_restored_in_case_the_write_was_partial() {
        let mut writer = FailFirstWrite::default();
        let mut enabled = false;

        assert!(enable_output_mode(&mut writer, &mut enabled, EnableBracketedPaste).is_err());
        assert!(enabled);

        let mut first_error = None;
        restore_output_mode(
            &mut writer,
            &mut enabled,
            DisableBracketedPaste,
            &mut first_error,
        );
        assert!(first_error.is_none());
        assert!(!enabled);
        assert!(writer.output.ends_with(b"\x1b[?2004l"));
    }

    #[test]
    fn synchronized_update_commands_are_balanced() {
        let mut output = Vec::new();
        execute!(
            output,
            BeginSynchronizedUpdate,
            Print("frame"),
            EndSynchronizedUpdate
        )
        .unwrap();

        assert_eq!(output, b"\x1b[?2026hframe\x1b[?2026l");
    }

    #[test]
    fn first_frame_is_committed_to_scrollback() {
        let mut tracker = ScrollbackTracker::default();

        let append = tracker
            .update(Some(scrollback_frame("one", 8, &["first", "second"])), 10)
            .unwrap();

        assert_eq!(append.x, 1);
        assert_eq!(buffer_lines(&append.rows), ["first", "second"]);
    }

    #[test]
    fn appended_rows_are_committed_without_replaying_the_prefix() {
        let mut tracker = ScrollbackTracker::default();
        tracker.update(Some(scrollback_frame("one", 8, &["first"])), 10);

        let append = tracker
            .update(
                Some(scrollback_frame("one", 8, &["first", "second", "third"])),
                10,
            )
            .unwrap();

        assert_eq!(buffer_lines(&append.rows), ["second", "third"]);
    }

    #[test]
    fn streaming_change_replaces_tracking_state_without_replaying_old_rows() {
        let mut tracker = ScrollbackTracker::default();
        tracker.update(Some(scrollback_frame("one", 8, &["stable", "draft"])), 10);

        assert!(
            tracker
                .update(Some(scrollback_frame("one", 8, &["stable", "changed"])), 10,)
                .is_none()
        );
        let append = tracker
            .update(
                Some(scrollback_frame("one", 8, &["stable", "changed", "done"])),
                10,
            )
            .unwrap();
        assert_eq!(buffer_lines(&append.rows), ["done"]);
    }

    #[test]
    fn content_shrink_does_not_duplicate_rows_when_it_grows_back() {
        let mut tracker = ScrollbackTracker::default();
        tracker.update(
            Some(scrollback_frame("one", 8, &["one", "two", "three"])),
            10,
        );

        assert!(
            tracker
                .update(Some(scrollback_frame("one", 8, &["one", "two"])), 10)
                .is_none()
        );
        assert!(
            tracker
                .update(
                    Some(scrollback_frame("one", 8, &["one", "two", "three"])),
                    10,
                )
                .is_none()
        );
    }

    #[test]
    fn taller_bottom_panel_commits_only_newly_hidden_rows() {
        let mut tracker = ScrollbackTracker::default();
        tracker.update(Some(scrollback_frame("one", 8, &["one"])), 10);

        let append = tracker
            .update(Some(scrollback_frame("one", 8, &["one", "two"])), 10)
            .unwrap();

        assert_eq!(buffer_lines(&append.rows), ["two"]);
    }

    #[test]
    fn resize_resets_wrapping_without_replaying_existing_scrollback() {
        let mut tracker = ScrollbackTracker::default();
        tracker.update(Some(scrollback_frame("one", 8, &["one", "two"])), 10);

        assert!(
            tracker
                .update(Some(scrollback_frame("one", 6, &["one", "two"])), 8)
                .is_none()
        );
        let append = tracker
            .update(
                Some(scrollback_frame("one", 6, &["one", "two", "three"])),
                8,
            )
            .unwrap();
        assert_eq!(buffer_lines(&append.rows), ["three"]);
    }

    #[test]
    fn switching_sessions_commits_the_selected_sessions_history() {
        let mut tracker = ScrollbackTracker::default();
        tracker.update(Some(scrollback_frame("one", 8, &["old"])), 10);

        let append = tracker
            .update(
                Some(scrollback_frame("two", 8, &["new one", "new two"])),
                10,
            )
            .unwrap();

        assert_eq!(buffer_lines(&append.rows), ["new one", "new two"]);
    }
}
