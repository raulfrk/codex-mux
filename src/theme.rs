//! Built-in terminal-safe visual themes.

use ratatui::style::{Color, Modifier, Style};

use crate::domain::ThemeId;

/// Concrete colors and modifiers used by the renderer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Theme {
    /// Stable theme identifier.
    pub id: ThemeId,
    /// Base text style.
    pub text: Style,
    /// Muted metadata style.
    pub muted: Style,
    /// Accent used for borders and key hints.
    pub accent: Style,
    /// Selected-row style.
    pub selected: Style,
    /// Warning and destructive-action style.
    pub warning: Style,
}

/// Resolves a built-in visual profile.
#[must_use]
pub fn theme(theme: ThemeId) -> Theme {
    let (accent, selected_foreground, selected_background, warning) = match theme {
        ThemeId::AdaptiveCyan => (Color::Cyan, Color::Black, Color::Cyan, Color::Yellow),
        ThemeId::BlueCommandPalette => {
            (Color::LightBlue, Color::White, Color::Blue, Color::LightRed)
        }
        ThemeId::AmberOperator => (Color::Yellow, Color::Black, Color::Yellow, Color::LightRed),
        ThemeId::EmberOrange => (
            Color::Rgb(255, 135, 48),
            Color::Black,
            Color::Rgb(255, 135, 48),
            Color::LightRed,
        ),
        ThemeId::Monochrome => (Color::Reset, Color::Reset, Color::Reset, Color::Reset),
    };
    let monochrome = theme == ThemeId::Monochrome;
    Theme {
        id: theme,
        text: Style::default(),
        muted: if monochrome {
            Style::default().add_modifier(Modifier::DIM)
        } else {
            Style::default().fg(Color::DarkGray)
        },
        accent: Style::default().fg(accent).add_modifier(Modifier::BOLD),
        selected: if monochrome {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
                .fg(selected_foreground)
                .bg(selected_background)
                .add_modifier(Modifier::BOLD)
        },
        warning: Style::default().fg(warning).add_modifier(Modifier::BOLD),
    }
}
