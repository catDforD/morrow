use ratatui::style::{Color, Modifier, Style};

pub const ACCENT: Color = Color::Rgb(0x8a, 0x86, 0xff);
pub const TEXT: Color = Color::Rgb(0xf4, 0xf4, 0xf7);
pub const MUTED: Color = Color::Rgb(0xaa, 0xaa, 0xbd);
pub const USER_BACKGROUND: Color = Color::Rgb(0x36, 0x35, 0x57);
pub const PENDING_BACKGROUND: Color = Color::Rgb(0x2b, 0x2a, 0x45);
pub const SUCCESS_BACKGROUND: Color = Color::Rgb(0x24, 0x4c, 0x32);
pub const ERROR_BACKGROUND: Color = Color::Rgb(0x5a, 0x2d, 0x35);
pub const SUCCESS: Color = Color::Rgb(0x4a, 0xde, 0x80);
pub const WARNING: Color = Color::Rgb(0xfd, 0xe0, 0x47);
pub const ERROR: Color = Color::Rgb(0xf8, 0x71, 0x71);
pub const INFO: Color = Color::Rgb(0x7d, 0xd3, 0xfc);

fn foreground(no_color: bool, color: Color) -> Style {
    if no_color {
        Style::default()
    } else {
        Style::default().fg(color)
    }
}

fn background(no_color: bool, color: Color) -> Style {
    if no_color {
        Style::default()
    } else {
        Style::default().bg(color)
    }
}

pub fn text(no_color: bool) -> Style {
    foreground(no_color, TEXT)
}

pub fn emphasis(no_color: bool) -> Style {
    text(no_color).add_modifier(Modifier::BOLD)
}

pub fn accent(no_color: bool) -> Style {
    foreground(no_color, ACCENT)
}

pub fn info(no_color: bool) -> Style {
    foreground(no_color, INFO)
}

pub fn muted(no_color: bool) -> Style {
    foreground(no_color, MUTED)
}

pub fn warning(no_color: bool) -> Style {
    foreground(no_color, WARNING)
}

pub fn error(no_color: bool) -> Style {
    foreground(no_color, ERROR)
}

pub fn success(no_color: bool) -> Style {
    foreground(no_color, SUCCESS)
}

pub fn selected(no_color: bool) -> Style {
    if no_color {
        Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED)
    } else {
        Style::default()
            .fg(TEXT)
            .bg(USER_BACKGROUND)
            .add_modifier(Modifier::BOLD)
    }
}

pub fn user_card(no_color: bool) -> Style {
    background(no_color, USER_BACKGROUND)
}

pub fn tool_pending(no_color: bool) -> Style {
    background(no_color, PENDING_BACKGROUND)
}

pub fn tool_success(no_color: bool) -> Style {
    background(no_color, SUCCESS_BACKGROUND)
}

pub fn tool_error(no_color: bool) -> Style {
    background(no_color, ERROR_BACKGROUND)
}

pub fn thinking(no_color: bool) -> Style {
    muted(no_color).add_modifier(Modifier::ITALIC)
}
