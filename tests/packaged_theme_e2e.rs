mod theme_e2e_support;

use std::{fs, thread, time::Duration};

use theme_e2e_support::{ThemeFixture, assert_screen_data, packaged_binary, wait_for_exact_file};

const THEMES: [(&str, &str); 5] = [
    ("adaptive-cyan", "38;5;6;49m"),
    ("blue-command-palette", "38;5;12;49m"),
    ("amber-operator", "38;5;3;49m"),
    ("ember-orange", "38;2;255;135;48;49m"),
    ("monochrome", "\u{1b}[7m"),
];

#[test]
fn every_persisted_theme_renders_the_same_session_and_action_data() {
    let Some(binary) = packaged_binary() else {
        return;
    };
    let mut fixture = ThemeFixture::new("persisted-matrix", binary);

    for (name, style_marker) in THEMES {
        fixture.write_config(format!("theme = \"{name}\"\n").as_bytes());
        let mut popup = fixture.popup(120, 40, false);
        popup.wait_for_text("theme-agent-thread");
        let screen = popup.wait_for_text("themes");
        assert_screen_data(&screen, &fixture.project_path());
        assert!(
            String::from_utf8_lossy(&screen).contains(style_marker),
            "theme {name} did not emit its expected terminal style"
        );
        popup.send(b"q");
        popup.wait_for_exit();
    }
}

#[test]
fn picker_live_previews_every_theme_and_enter_persists_atomically_then_reloads() {
    let _evidence = journey_evidence::journey(&["theme"]);
    let Some(binary) = packaged_binary() else {
        return;
    };
    let mut fixture = ThemeFixture::new("picker-apply", binary);
    fixture.write_config(b"theme = \"adaptive-cyan\"\n");
    let config = fixture.config_path();
    let mut popup = fixture.popup(120, 40, false);
    let mut screen = popup.wait_for_text("theme-agent-thread");
    popup.send(b"t");
    screen = popup.wait_for_growth(screen.len());
    popup.wait_for_text("theme · live preview");

    for (_, expected_style) in THEMES.into_iter().skip(1) {
        let before = screen.len();
        popup.send(b"j");
        screen = popup.wait_for_appended_text(before, expected_style);
    }

    popup.send(b"\r");
    wait_for_exact_file(
        &config,
        b"theme = \"monochrome\"\nsmart_naming = false\n\n[[profiles]]\nname = \"standard\"\nkey = \"s\"\npermissions = \"standard\"\n\n[[profiles]]\nname = \"yolo\"\nkey = \"y\"\npermissions = \"yolo\"\n",
    );
    let directory = config.parent().unwrap();
    assert_eq!(
        fs::read_dir(directory).unwrap().count(),
        1,
        "atomic theme save left a temporary file"
    );
    popup.send(b"q");
    popup.wait_for_exit();

    let mut restarted = fixture.popup(120, 40, false);
    let screen = restarted.wait_for_text("theme-agent-thread");
    assert!(String::from_utf8_lossy(&screen).contains("\u{1b}[7m"));
    restarted.send(b"q");
    restarted.wait_for_exit();
}

#[test]
fn escape_and_q_cancel_picker_without_changing_preference_bytes() {
    let Some(binary) = packaged_binary() else {
        return;
    };
    let mut fixture = ThemeFixture::new("picker-cancel", binary);
    let original = b"# byte identity matters\ntheme = \"ember-orange\"\n";
    fixture.write_config(original);

    let mut escape = fixture.popup(120, 40, false);
    escape.wait_for_text("theme-agent-thread");
    escape.send(b"tj");
    escape.wait_for_text("theme · live preview");
    escape.send(b"\x1b");
    thread::sleep(Duration::from_millis(1_200));
    escape.send(b"q");
    escape.wait_for_exit();
    assert_eq!(fs::read(fixture.config_path()).unwrap(), original);

    let mut q = fixture.popup(120, 40, false);
    q.wait_for_text("theme-agent-thread");
    q.send(b"tj");
    q.wait_for_text("theme · live preview");
    q.send(b"q");
    thread::sleep(Duration::from_millis(100));
    q.send(b"q");
    q.wait_for_exit();
    assert_eq!(fs::read(fixture.config_path()).unwrap(), original);
}

#[test]
fn malformed_and_unreadable_preferences_warn_fall_back_and_remain_usable() {
    let Some(binary) = packaged_binary() else {
        return;
    };

    let mut malformed = ThemeFixture::new("malformed", binary.clone());
    let malformed_bytes = b"theme = \"ultraviolet\"\n";
    malformed.write_config(malformed_bytes);
    let mut popup = malformed.popup(62, 35, false);
    popup.wait_for_text("could not load configuration");
    let screen = popup.wait_for_text("theme-agent-thread");
    let screen = String::from_utf8_lossy(&screen);
    assert!(screen.contains("theme-agent-thread"));
    assert!(
        screen.contains("38;5;6;49m"),
        "fallback was not Adaptive Cyan"
    );
    popup.send(b"q");
    popup.wait_for_exit();
    assert_eq!(fs::read(malformed.config_path()).unwrap(), malformed_bytes);

    let mut unreadable = ThemeFixture::new("unreadable", binary);
    let config = unreadable.config_path();
    fs::create_dir_all(&config).unwrap();
    let mut popup = unreadable.popup(62, 35, false);
    popup.wait_for_text("could not load configuration");
    let screen = popup.wait_for_text("theme-agent-thread");
    let screen = String::from_utf8_lossy(&screen);
    assert!(screen.contains("theme-agent-thread"));
    assert!(
        screen.contains("38;5;6;49m"),
        "fallback was not Adaptive Cyan"
    );
    popup.send(b"q");
    popup.wait_for_exit();
    assert!(config.is_dir(), "unreadable preference path was mutated");
}

#[test]
fn no_color_forces_monochrome_disables_picker_and_preserves_ember_bytes() {
    let Some(binary) = packaged_binary() else {
        return;
    };
    let mut fixture = ThemeFixture::new("no-color", binary);
    let original = b"# keep saved color\ntheme = \"ember-orange\"\n";
    fixture.write_config(original);
    let mut popup = fixture.popup(120, 40, true);
    let initial = popup.wait_for_text("theme-agent-thread");
    let initial = String::from_utf8_lossy(&initial);
    assert!(initial.contains("\u{1b}[7m"), "NO_COLOR was not monochrome");
    assert!(
        !initial.contains("255;135;48"),
        "NO_COLOR emitted the saved orange style"
    );
    popup.send(b"t");
    thread::sleep(Duration::from_millis(150));
    popup.send(b"q");
    let screen = popup.wait_for_exit();
    assert!(
        !String::from_utf8_lossy(&screen).contains("theme · live preview"),
        "NO_COLOR allowed the theme picker"
    );
    assert_eq!(fs::read(fixture.config_path()).unwrap(), original);
}
mod journey_evidence;
