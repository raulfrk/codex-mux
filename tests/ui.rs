use std::path::PathBuf;

use codex_mux::{
    config::{LaunchProfile, PermissionPreset},
    domain::{Pane, PaneId, SessionId, ThemeId},
    ui::{Action, App, ColorPolicy, render},
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};

fn pane(id: &str, title: &str, path: &str) -> Pane {
    Pane {
        id: PaneId::new(id).unwrap(),
        session_id: SessionId::new("$1").unwrap(),
        title: Some(title.to_owned()),
        generated_title: None,
        generated_at_unix: None,
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
    assert_eq!(app.handle_key(key(KeyCode::Char('n'))), None);
    assert_eq!(
        app.handle_key(key(KeyCode::Char('s'))),
        Some(Action::LaunchProfile(LaunchProfile::standard()))
    );
    app.handle_key(key(KeyCode::Esc));
    assert_eq!(
        app.handle_key(key(KeyCode::Char('r'))),
        Some(Action::Resume)
    );
    assert_eq!(app.handle_key(key(KeyCode::Char('q'))), Some(Action::Quit));
    assert_eq!(app.handle_key(key(KeyCode::Esc)), Some(Action::Quit));
}

#[test]
fn profile_picker_supports_fast_keys_and_creating_a_yolo_profile() {
    let mut app = App::new(vec![], ThemeId::AdaptiveCyan, None);
    app.handle_key(key(KeyCode::Char('n')));
    assert_eq!(
        app.handle_key(key(KeyCode::Char('y'))),
        Some(Action::LaunchProfile(LaunchProfile::yolo()))
    );

    app.handle_key(key(KeyCode::Esc));
    app.handle_key(key(KeyCode::Char('n')));
    app.handle_key(key(KeyCode::Char('a')));
    for character in "fast".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
    app.handle_key(key(KeyCode::Tab));
    app.handle_key(key(KeyCode::Char('f')));
    app.handle_key(key(KeyCode::Tab));
    app.handle_key(key(KeyCode::Tab));
    app.handle_key(key(KeyCode::Right));
    let action = app.handle_key(key(KeyCode::Enter));
    let Some(Action::PersistProfiles(profiles)) = action else {
        panic!("editor did not request profile persistence");
    };
    assert_eq!(profiles.len(), 3);
    assert_eq!(profiles[2].name, "fast");
    assert_eq!(profiles[2].key, 'f');
    assert_eq!(profiles[2].permissions, PermissionPreset::Yolo);
    app.profiles_saved(profiles.clone());
    assert_eq!(
        app.handle_key(key(KeyCode::Enter)),
        Some(Action::LaunchProfile(profiles[2].clone()))
    );
}

#[test]
fn profile_picker_and_editor_render_in_every_theme() {
    for theme in ThemeId::ALL {
        let mut app = App::new(vec![], theme, None);
        app.handle_key(key(KeyCode::Char('n')));
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &app)).unwrap();
        let picker = terminal.backend().buffer();
        let picker_text = picker
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(
            picker_text.contains("launch profile"),
            "missing picker for {theme:?}"
        );

        app.handle_key(key(KeyCode::Char('a')));
        terminal.draw(|frame| render(frame, &app)).unwrap();
        let editor = terminal.backend().buffer();
        let editor_text = editor
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(
            editor_text.contains("add profile"),
            "missing editor for {theme:?}"
        );
    }
}

#[test]
fn constrained_picker_scrolls_to_keep_a_long_list_selection_visible() {
    let keys = ['s', 'y', 'c', 'd', 'f', 'g', 'h', 'i', 'l', 'm'];
    let profiles = keys
        .into_iter()
        .enumerate()
        .map(|(index, key)| LaunchProfile {
            name: format!("profile-{index}"),
            key,
            executable: None,
            permissions: PermissionPreset::Standard,
        })
        .collect();
    let mut app = App::with_profiles(
        vec![],
        ThemeId::AdaptiveCyan,
        None,
        ColorPolicy::Allow,
        profiles,
    );
    app.handle_key(key(KeyCode::Char('n')));
    for _ in 0..9 {
        app.handle_key(key(KeyCode::Down));
    }

    let backend = TestBackend::new(40, 8);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| render(frame, &app)).unwrap();
    let screen = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(screen.contains("profile-9"));
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
    assert!(first.contains("n r x t c q"));
}

#[test]
fn configuration_panel_explains_and_toggles_smart_naming() {
    let mut app = App::new(vec![], ThemeId::AdaptiveCyan, None);
    assert!(!app.smart_naming_enabled());
    app.handle_key(key(KeyCode::Char('C')));
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| render(frame, &app)).unwrap();
    let text = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(text.contains("Conversation-aware names"));
    assert!(text.contains("OFF (default)"));
    assert!(text.contains("completed Codex conversation"));
    assert_eq!(
        app.handle_key(key(KeyCode::Char('N'))),
        Some(Action::PersistSmartNaming(true))
    );
    app.smart_naming_saved(true);
    assert!(app.smart_naming_enabled());
}

#[test]
fn coherent_inventory_refresh_preserves_selection_and_recovers_from_failure() {
    let mut app = App::new(
        vec![
            pane("%1", "one", "/work/one"),
            pane("%2", "two", "/work/two"),
        ],
        ThemeId::default(),
        None,
    );
    app.select_pane(&PaneId::new("%2").unwrap());
    app.inventory_failed("slow tmux failed");

    app.inventory_refreshed(vec![
        pane("%2", "two refreshed", "/work/two"),
        pane("%3", "three", "/work/three"),
    ]);

    assert_eq!(app.selected_pane_id(), Some(&PaneId::new("%2").unwrap()));
    assert_eq!(app.panes()[0].display_title(), "two refreshed");
}

#[test]
fn configuration_disclosure_and_controls_remain_visible_on_phone_and_tiny_screens() {
    for (width, height) in [(62, 20), (32, 8)] {
        let mut app = App::new(vec![], ThemeId::AdaptiveCyan, None);
        app.handle_key(key(KeyCode::Char('c')));
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &app)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("Smart names: OFF"));
        assert!(text.contains("GPT-5.6 Luna"));
        assert!(
            text.contains("stores no chat")
                || text.contains("No chat stored")
                || text.contains("not stored")
        );
        assert!(text.contains("N toggle"));
    }
}

#[test]
fn naming_persistence_failure_rolls_back_and_success_clears_its_warning() {
    let mut app = App::new(vec![], ThemeId::AdaptiveCyan, None);
    app.handle_key(key(KeyCode::Char('c')));
    assert_eq!(
        app.handle_key(key(KeyCode::Char('n'))),
        Some(Action::PersistSmartNaming(true))
    );
    app.smart_naming_save_failed("disk full");
    assert!(!app.smart_naming_enabled());
    assert_eq!(
        app.handle_key(key(KeyCode::Char('n'))),
        Some(Action::PersistSmartNaming(true))
    );
    app.smart_naming_saved(true);
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| render(frame, &app)).unwrap();
    let text = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(!text.contains("disk full"));
}
