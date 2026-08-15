use std::path::PathBuf;

use codex_mux::{
    domain::{Pane, PaneId, SessionId, ThemeId},
    ui::{Action, App, render},
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};

fn pane(id: &str, title: &str, path: &str) -> Pane {
    Pane {
        id: PaneId::new(id).unwrap(),
        session_id: SessionId::new("$1").unwrap(),
        title: Some(title.to_owned()),
        current_path: PathBuf::from(path),
    }
}

fn rendered(width: u16, height: u16) -> String {
    let app = App::new(
        vec![
            pane("%19", "shipping feature", "/work/shipping"),
            pane("%83", "review release", "/work/release"),
        ],
        ThemeId::EmberOrange,
        None,
    );
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| render(frame, &app)).unwrap();
    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

#[test]
fn wide_and_both_compact_thresholds_render_deterministically() {
    let wide = rendered(120, 30);
    assert!(wide.contains("Commands"));
    assert!(!wide.contains("current_path"));
    assert_eq!(wide.matches("/work/shipping").count(), 1);
    assert_eq!(wide.matches("/work/release").count(), 1);

    let width_compact = rendered(89, 28);
    assert!(width_compact.contains("Enter switch"));
    assert!(!width_compact.contains("Commands"));

    let height_compact = rendered(120, 27);
    assert!(height_compact.contains("Enter switch"));
    assert!(!height_compact.contains("Commands"));
    assert_eq!(height_compact, rendered(120, 27));
}

#[test]
fn documented_browse_keys_produce_their_actions() {
    let mut app = App::new(
        vec![
            pane("%19", "shipping feature", "/work/shipping"),
            pane("%83", "review release", "/work/release"),
        ],
        ThemeId::AdaptiveCyan,
        None,
    );
    app.handle_key(key(KeyCode::Down));
    assert_eq!(app.selected_pane_id().unwrap().as_str(), "%83");
    app.handle_key(key(KeyCode::Up));
    assert_eq!(app.selected_pane_id().unwrap().as_str(), "%19");
    assert_eq!(
        app.handle_key(key(KeyCode::Enter)),
        Some(Action::Activate(PaneId::new("%19").unwrap()))
    );
    assert_eq!(app.handle_key(key(KeyCode::Char('n'))), Some(Action::New));
    assert_eq!(
        app.handle_key(key(KeyCode::Char('r'))),
        Some(Action::Resume)
    );
    assert_eq!(app.handle_key(key(KeyCode::Char('q'))), Some(Action::Quit));
    assert_eq!(app.handle_key(key(KeyCode::Esc)), Some(Action::Quit));
}

#[test]
fn phone_layout_at_62_columns_is_readable_and_hides_internal_pane_ids() {
    let screen = rendered(62, 20);
    assert!(screen.contains("shipping feature"));
    assert!(screen.contains("Enter open"));
    assert!(!screen.contains("%19"));
    assert!(!screen.contains("%83"));
}

#[test]
fn tiny_layout_is_deterministic_and_keeps_primary_controls() {
    let first = rendered(32, 8);
    let second = rendered(32, 8);
    assert_eq!(first, second);
    assert!(first.contains("shipping feature"));
    assert!(first.contains("n r x t q"));
}
