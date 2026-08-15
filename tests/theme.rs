use std::{fs, path::PathBuf, time::SystemTime};

use codex_mux::{
    config::XdgThemeStore,
    domain::{ThemeId, ThemeStore},
    theme::theme,
};

fn temporary_config(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir()
        .join(format!("codex-mux-{name}-{}-{nonce}", std::process::id()))
        .join("config.toml")
}

#[test]
fn saved_theme_round_trips_through_an_atomic_file() {
    let path = temporary_config("round-trip");
    let store = XdgThemeStore::at(&path);
    store.save(ThemeId::EmberOrange).unwrap();

    assert_eq!(store.load().unwrap(), Some(ThemeId::EmberOrange));
    assert!(fs::read_to_string(&path).unwrap().contains("ember-orange"));
    let directory = path.parent().unwrap();
    assert_eq!(fs::read_dir(directory).unwrap().count(), 1);

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn malformed_and_unreadable_preferences_warn_and_fall_back() {
    let malformed = temporary_config("malformed");
    fs::create_dir_all(malformed.parent().unwrap()).unwrap();
    fs::write(&malformed, "theme = 'ultraviolet'\n").unwrap();
    let preference = XdgThemeStore::at(&malformed).load_preference();
    assert_eq!(preference.selected, ThemeId::AdaptiveCyan);
    assert!(preference.warning.unwrap().contains("could not parse"));
    fs::remove_dir_all(malformed.parent().unwrap()).unwrap();

    let unreadable = temporary_config("unreadable");
    fs::create_dir_all(&unreadable).unwrap();
    let preference = XdgThemeStore::at(&unreadable).load_preference();
    assert_eq!(preference.selected, ThemeId::AdaptiveCyan);
    assert!(preference.warning.unwrap().contains("could not read"));
    fs::remove_dir_all(&unreadable).unwrap();
}

#[test]
fn no_color_is_an_invocation_override_not_a_saved_preference() {
    let path = temporary_config("no-color");
    let store = XdgThemeStore::at(&path);
    store.save(ThemeId::EmberOrange).unwrap();

    let preference = store.load_preference();
    assert_eq!(preference.effective_theme(true), ThemeId::Monochrome);
    assert_eq!(store.load().unwrap(), Some(ThemeId::EmberOrange));

    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn all_five_builtin_themes_resolve_and_monochrome_avoids_color_pairs() {
    assert_eq!(ThemeId::ALL.len(), 5);
    for id in ThemeId::ALL {
        assert_eq!(theme(id).id, id);
    }
    let monochrome = theme(ThemeId::Monochrome);
    assert_eq!(monochrome.selected.fg, None);
    assert_eq!(monochrome.selected.bg, None);
}
