use std::io::{self, Stdout, Write};

#[cfg(unix)]
use crossterm::event::{
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::{
    Command,
    cursor::{Hide, Show},
    event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

pub type MorrowTerminal = Terminal<CrosstermBackend<Stdout>>;

/// Owns every terminal mode enabled by Morrow and restores it on all return and panic paths.
pub struct TerminalGuard {
    terminal: Option<MorrowTerminal>,
    modes: TerminalModes,
}

#[derive(Debug, Default)]
struct TerminalModes {
    raw_mode_enabled: bool,
    alternate_screen_enabled: bool,
    mouse_capture_enabled: bool,
    bracketed_paste_enabled: bool,
    cursor_hidden: bool,
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
        restore_output_mode(writer, &mut self.cursor_hidden, Show, &mut first_error);
        restore_output_mode(
            writer,
            &mut self.bracketed_paste_enabled,
            DisableBracketedPaste,
            &mut first_error,
        );
        restore_output_mode(
            writer,
            &mut self.mouse_capture_enabled,
            DisableMouseCapture,
            &mut first_error,
        );
        restore_output_mode(
            writer,
            &mut self.alternate_screen_enabled,
            LeaveAlternateScreen,
            &mut first_error,
        );
        first_error.map_or(Ok(()), Err)
    }

    fn output_modes_restored(&self) -> bool {
        !self.alternate_screen_enabled
            && !self.mouse_capture_enabled
            && !self.bracketed_paste_enabled
            && !self.cursor_hidden
            && {
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
    execute!(writer, command)?;
    *enabled = true;
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
                &mut modes.alternate_screen_enabled,
                EnterAlternateScreen,
            )?;
            enable_output_mode(
                &mut stdout,
                &mut modes.mouse_capture_enabled,
                EnableMouseCapture,
            )?;
            enable_output_mode(
                &mut stdout,
                &mut modes.bracketed_paste_enabled,
                EnableBracketedPaste,
            )?;
            enable_output_mode(&mut stdout, &mut modes.cursor_hidden, Hide)?;
            #[cfg(unix)]
            enable_output_mode(
                &mut stdout,
                &mut modes.keyboard_enhancement_enabled,
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
            )?;
            Ok::<(), io::Error>(())
        })();
        if let Err(error) = setup {
            let _ = modes.restore_output_modes(&mut stdout);
            if modes.raw_mode_enabled {
                let _ = disable_raw_mode();
            }
            return Err(error);
        }
        let terminal = match Terminal::new(CrosstermBackend::new(stdout)) {
            Ok(terminal) => terminal,
            Err(error) => {
                let mut stdout = io::stdout();
                let _ = modes.restore_output_modes(&mut stdout);
                if modes.raw_mode_enabled {
                    let _ = disable_raw_mode();
                }
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
        if let Some(terminal) = self.terminal.as_mut()
            && let Err(error) = self.modes.restore_output_modes(terminal.backend_mut())
        {
            first_error = Some(error);
        }
        if self.modes.raw_mode_enabled {
            match disable_raw_mode() {
                Ok(()) => self.modes.raw_mode_enabled = false,
                Err(error) if first_error.is_none() => first_error = Some(error),
                Err(_) => {}
            }
        }
        if self.modes.output_modes_restored() {
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
            alternate_screen_enabled: true,
            mouse_capture_enabled: true,
            bracketed_paste_enabled: true,
            cursor_hidden: true,
            #[cfg(unix)]
            keyboard_enhancement_enabled: false,
            raw_mode_enabled: false,
        };
        let mut writer = FailFirstWrite::default();

        assert!(modes.restore_output_modes(&mut writer).is_err());
        assert!(modes.cursor_hidden);
        assert!(!modes.alternate_screen_enabled);
        assert!(!modes.mouse_capture_enabled);
        assert!(!modes.bracketed_paste_enabled);

        modes.restore_output_modes(&mut writer).unwrap();
        assert!(modes.output_modes_restored());
        assert!(writer.output.ends_with(b"\x1b[?25h"));
    }
}
