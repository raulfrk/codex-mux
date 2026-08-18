use std::path::PathBuf;

use codex_mux::{
    config::{LaunchProfile, PermissionPreset},
    domain::{Pane, PaneId, SessionId, ThemeId},
    ui::{Action, App, ColorPolicy, render},
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};
use unicode_width::UnicodeWidthStr;

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
    rendered_panes(
        vec![
            pane("%19", "shipping feature", "/work/shipping"),
            pane("%83", "review release", "/work/release"),
        ],
        width,
        height,
    )
}

fn rendered_panes(panes: Vec<Pane>, width: u16, height: u16) -> String {
    rendered_app(&App::new(panes, ThemeId::EmberOrange, None), width, height)
}

fn rendered_app(app: &App, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| render(frame, app)).unwrap();
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
    assert_eq!(app.handle_key(key(KeyCode::Char('r'))), None);
    assert_eq!(
        app.handle_key(key(KeyCode::Char('s'))),
        Some(Action::Resume)
    );
    assert_eq!(app.resume_profile(), Some(&LaunchProfile::standard()));
    app.handle_key(key(KeyCode::Esc));
    assert_eq!(app.handle_key(key(KeyCode::Char('q'))), Some(Action::Quit));
    assert_eq!(app.handle_key(key(KeyCode::Esc)), Some(Action::Quit));
}

#[test]
fn editing_a_profile_from_resume_retains_resume_intent_after_save() {
    let mut app = App::new(vec![], ThemeId::AdaptiveCyan, None);
    app.handle_key(key(KeyCode::Char('r')));
    app.handle_key(key(KeyCode::Char('e')));
    let Some(Action::PersistProfiles(profiles)) = app.handle_key(key(KeyCode::Enter)) else {
        panic!("editor did not request profile persistence");
    };
    app.profiles_saved(profiles.clone());

    assert_eq!(app.handle_key(key(KeyCode::Enter)), Some(Action::Resume));
    assert_eq!(app.resume_profile(), Some(&profiles[0]));
}

#[test]
fn resume_profile_navigation_and_editor_cancel_retain_resume_intent() {
    let mut app = App::new(vec![], ThemeId::AdaptiveCyan, None);
    app.handle_key(key(KeyCode::Char('r')));
    app.handle_key(key(KeyCode::Down));
    app.handle_key(key(KeyCode::Char('e')));
    app.handle_key(key(KeyCode::Esc));

    assert_eq!(app.handle_key(key(KeyCode::Enter)), Some(Action::Resume));
    assert_eq!(app.resume_profile(), Some(&LaunchProfile::yolo()));
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
    let lines = screen.lines().collect::<Vec<_>>();
    let title_line = lines
        .iter()
        .position(|line| line.contains("shipping feature"))
        .unwrap();
    let path_line = lines
        .iter()
        .position(|line| line.contains("/work/shipping"))
        .unwrap();
    assert_eq!(path_line, title_line + 1);
    assert!(screen.contains("Enter open"));
    assert!(!screen.contains("%19"));
    assert!(!screen.contains("%83"));
}

#[test]
fn narrow_rows_wire_directional_unicode_elision_into_the_renderer() {
    let screen = rendered_panes(
        vec![pane(
            "%19",
            "Improve e\u{301} Unicode session row truncation across narrow terminals",
            "/home/raul/workspace/very/deep/repository/session-rows",
        )],
        40,
        20,
    );
    let lines = screen.lines().collect::<Vec<_>>();
    let title_index = lines
        .iter()
        .position(|line| line.contains("Improve"))
        .unwrap();
    let title = lines[title_index].strip_prefix("› ").unwrap().trim_end();
    let path_line = lines[title_index + 1];
    let path = path_line.strip_prefix("    ").unwrap().trim_end();

    assert!(title.ends_with('…'), "title must end-elide: {title:?}");
    assert!(path.starts_with('…'), "path must start-elide: {path:?}");
    assert!(
        path.ends_with("session-rows"),
        "path tail was lost: {path:?}"
    );
    assert!(UnicodeWidthStr::width(title) + 2 <= 40);
    assert!(UnicodeWidthStr::width(path) + 4 <= 40);
    assert!(!screen.contains("narrow terminals"));
    assert!(!screen.contains("/home/raul"));
}

#[test]
fn approved_sizes_preserve_adaptive_title_and_path_contract() {
    let long_title = concat!(
        "Improve e\u{301} Unicode session rows across wide compact and phone ",
        "terminal layouts while preserving a useful conversation summary"
    );
    let deep_path = concat!(
        "/home/raul/workspace/改善/very/deep/project/with/many/components/",
        "adaptive/session-row-renderer"
    );

    for (width, height) in [(120, 30), (89, 28), (62, 20)] {
        let screen = rendered_panes(vec![pane("%19", long_title, deep_path)], width, height);
        let lines = screen.lines().collect::<Vec<_>>();
        let title_index = lines
            .iter()
            .position(|line| line.contains("Improve e\u{301}"))
            .unwrap_or_else(|| panic!("missing title at {width}x{height}"));
        let title = lines[title_index]
            .split_once("› ")
            .unwrap()
            .1
            .split('│')
            .next()
            .unwrap()
            .trim_end();
        let path = lines[title_index + 1]
            .split('│')
            .find(|segment| segment.contains('…'))
            .unwrap()
            .trim();

        assert!(
            title.ends_with('…'),
            "title did not end-elide at {width}x{height}: {title:?}"
        );
        assert!(
            path.starts_with('…'),
            "path did not start-elide at {width}x{height}: {path:?}"
        );
        assert!(
            path.ends_with("session-row-renderer"),
            "path tail was lost at {width}x{height}: {path:?}"
        );
        assert!(UnicodeWidthStr::width(title) + 2 <= usize::from(width));
        // TestBackend exposes wide-glyph continuation cells as spaces in reconstructed rows,
        // so string-width measurement would double-count them. Ellipsis plus the retained tail
        // proves the renderer budget kept the useful path content inside the visible row.
    }

    let tiny = rendered_panes(vec![pane("%19", "selected conversation", deep_path)], 32, 8);
    assert!(tiny.contains("selected conversation"));
    assert!(!tiny.contains("session-row-renderer"));
    assert!(!tiny.contains("/home/raul"));
}

#[test]
fn constrained_session_list_scrolls_selected_final_two_line_row_into_view() {
    let panes = (0..7)
        .map(|index| {
            pane(
                &format!("%{}", index + 1),
                &format!("conversation-{index}"),
                &format!("/work/project-{index}"),
            )
        })
        .collect::<Vec<_>>();
    let mut app = App::new(panes, ThemeId::EmberOrange, None);
    for _ in 0..6 {
        app.handle_key(key(KeyCode::Down));
    }

    let screen = rendered_app(&app, 40, 12);
    let lines = screen.lines().collect::<Vec<_>>();
    let title_index = lines
        .iter()
        .position(|line| line.contains("conversation-6"))
        .expect("selected final title was not scrolled into view");

    assert!(lines[title_index].starts_with("› conversation-6"));
    assert!(lines[title_index + 1].contains("/work/project-6"));
    assert!(!screen.contains("conversation-0"));
    assert!(!screen.contains("/work/project-0"));
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
