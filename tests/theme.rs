use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::{Arc, Barrier},
    thread,
    time::SystemTime,
};

use codex_mux::{
    config::{
        LaunchProfile, PermissionPreset, ProcessSettings, XdgThemeStore, validate_process_settings,
        validate_profiles,
    },
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
    assert!(preference.warning.unwrap().contains("could not load"));
    fs::remove_dir_all(malformed.parent().unwrap()).unwrap();

    let unreadable = temporary_config("unreadable");
    fs::create_dir_all(&unreadable).unwrap();
    let preference = XdgThemeStore::at(&unreadable).load_preference();
    assert_eq!(preference.selected, ThemeId::AdaptiveCyan);
    assert!(preference.warning.unwrap().contains("could not load"));
    fs::remove_dir_all(&unreadable).unwrap();
}

#[test]
fn legacy_theme_only_config_receives_default_profiles() {
    let path = temporary_config("legacy-theme");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "theme = 'ember-orange'\n").unwrap();

    let preference = XdgThemeStore::at(&path).load_preference();
    assert_eq!(preference.selected, ThemeId::EmberOrange);
    assert_eq!(
        preference.profiles,
        vec![LaunchProfile::standard(), LaunchProfile::yolo()]
    );
    assert!(preference.warning.is_none());
    assert!(!preference.smart_naming);
    assert!(preference.process.is_none());

    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn process_configuration_requires_executable_absolute_paths_and_exact_commands() {
    let root = temporary_config("process-validation");
    fs::create_dir_all(root.parent().unwrap()).unwrap();
    let launcher = root.parent().unwrap().join("launcher");
    let underlying = root.parent().unwrap().join("codex");
    for executable in [&launcher, &underlying] {
        fs::write(executable, b"#!/bin/sh\n").unwrap();
        let mut permissions = fs::metadata(executable).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(executable, permissions).unwrap();
    }
    let valid = ProcessSettings {
        launch_executable: launcher,
        match_executables: vec![underlying],
        pane_commands: vec!["codex".to_owned()],
        match_scope: Default::default(),
        match_command_regexes: Vec::new(),
        pane_command_regexes: Vec::new(),
    };
    validate_process_settings(&valid).unwrap();
    let mut spaced = valid.clone();
    spaced.pane_commands = vec!["custom codex".to_owned()];
    validate_process_settings(&spaced).unwrap();
    let mut invalid = valid.clone();
    invalid.match_executables.clear();
    assert!(validate_process_settings(&invalid).is_err());
    invalid = valid.clone();
    invalid.pane_commands = vec!["codex; echo injected".to_owned()];
    assert!(validate_process_settings(&invalid).is_err());
    invalid = valid.clone();
    invalid.match_command_regexes = vec!["[".to_owned()];
    assert!(validate_process_settings(&invalid).is_err());
    invalid = valid.clone();
    invalid.pane_commands.clear();
    invalid.pane_command_regexes = vec!["^supervisor$".to_owned()];
    assert!(validate_process_settings(&invalid).is_ok());
    fs::remove_dir_all(root.parent().unwrap()).unwrap();
}

#[test]
fn smart_naming_defaults_off_and_round_trips_without_losing_other_settings() {
    let path = temporary_config("smart-naming");
    let store = XdgThemeStore::at(&path);
    assert!(!store.load_preference().smart_naming);
    store.save(ThemeId::EmberOrange).unwrap();
    store.save_smart_naming(true).unwrap();
    let preference = store.load_preference();
    assert!(preference.smart_naming);
    assert_eq!(preference.selected, ThemeId::EmberOrange);
    assert_eq!(
        preference.profiles,
        vec![LaunchProfile::standard(), LaunchProfile::yolo()]
    );
    store.save_profiles(&[LaunchProfile::standard()]).unwrap();
    assert!(store.load_preference().smart_naming);
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn profiles_round_trip_without_overwriting_the_saved_theme() {
    let path = temporary_config("profiles-round-trip");
    let store = XdgThemeStore::at(&path);
    store.save(ThemeId::BlueCommandPalette).unwrap();
    let profiles = vec![LaunchProfile {
        name: "careful".to_owned(),
        key: 'c',
        executable: None,
        permissions: PermissionPreset::Standard,
    }];
    store.save_profiles(&profiles).unwrap();

    let preference = store.load_preference();
    assert_eq!(preference.selected, ThemeId::BlueCommandPalette);
    assert_eq!(preference.profiles, profiles);

    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn profile_validation_rejects_collisions_and_unsafe_executables() {
    let duplicate = vec![
        LaunchProfile::standard(),
        LaunchProfile {
            name: "duplicate".to_owned(),
            key: 'S',
            executable: None,
            permissions: PermissionPreset::Yolo,
        },
    ];
    assert!(validate_profiles(&duplicate).is_err());

    let relative = vec![LaunchProfile {
        name: "relative".to_owned(),
        key: 'c',
        executable: Some(PathBuf::from("codex")),
        permissions: PermissionPreset::Standard,
    }];
    assert!(validate_profiles(&relative).is_err());

    let executable = temporary_config("executable").with_file_name("codex-custom");
    fs::create_dir_all(executable.parent().unwrap()).unwrap();
    fs::write(&executable, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let valid = vec![LaunchProfile {
        name: "custom".to_owned(),
        key: 'c',
        executable: Some(executable.clone()),
        permissions: PermissionPreset::Yolo,
    }];
    validate_profiles(&valid).unwrap();

    fs::remove_dir_all(executable.parent().unwrap()).unwrap();
}

#[test]
fn semantically_invalid_saved_profiles_fall_back_safely() {
    let path = temporary_config("invalid-profile");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        "theme = 'ember-orange'\n[[profiles]]\nname = 'reserved'\nkey = 'n'\n",
    )
    .unwrap();

    let preference = XdgThemeStore::at(&path).load_preference();
    assert!(!preference.was_saved);
    assert_eq!(preference.selected, ThemeId::AdaptiveCyan);
    assert_eq!(preference.profiles[0], LaunchProfile::standard());
    assert!(preference.warning.unwrap().contains("reserved"));

    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn component_saves_refuse_to_overwrite_an_invalid_existing_config() {
    let path = temporary_config("invalid-preserved");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let invalid = b"theme = 'ember-orange'\n[[profiles]\n";
    fs::write(&path, invalid).unwrap();
    let store = XdgThemeStore::at(&path);

    assert!(store.save(ThemeId::Monochrome).is_err());
    assert_eq!(fs::read(&path).unwrap(), invalid);
    assert!(store.save_profiles(&[LaunchProfile::standard()]).is_err());
    assert_eq!(fs::read(&path).unwrap(), invalid);

    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn profile_replacement_repairs_a_missing_old_executable_and_preserves_theme() {
    let path = temporary_config("repair-missing-executable");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        "theme = 'ember-orange'\n[[profiles]]\nname = 'gone'\nkey = 'c'\nexecutable = '/definitely/missing/codex'\n",
    )
    .unwrap();
    let store = XdgThemeStore::at(&path);

    store.save_profiles(&[LaunchProfile::standard()]).unwrap();

    let preference = store.load_preference();
    assert_eq!(preference.selected, ThemeId::EmberOrange);
    assert_eq!(preference.profiles, vec![LaunchProfile::standard()]);
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn concurrent_component_saves_preserve_both_updates() {
    for iteration in 0..20 {
        let path = temporary_config(&format!("concurrent-{iteration}"));
        let store = XdgThemeStore::at(&path);
        store.save(ThemeId::AdaptiveCyan).unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let theme_store = store.clone();
        let theme_barrier = barrier.clone();
        let theme = thread::spawn(move || {
            theme_barrier.wait();
            theme_store.save(ThemeId::EmberOrange).unwrap();
        });
        let profile_store = store.clone();
        let profile_barrier = barrier.clone();
        let profile = thread::spawn(move || {
            profile_barrier.wait();
            profile_store
                .save_profiles(&[LaunchProfile {
                    name: "fast".to_owned(),
                    key: 'f',
                    executable: None,
                    permissions: PermissionPreset::Yolo,
                }])
                .unwrap();
        });
        barrier.wait();
        theme.join().unwrap();
        profile.join().unwrap();

        let preference = store.load_preference();
        assert_eq!(preference.selected, ThemeId::EmberOrange);
        assert_eq!(preference.profiles[0].name, "fast");
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}

#[test]
fn profile_names_reject_terminal_control_characters() {
    let profiles = vec![LaunchProfile {
        name: "spoof\nnext row\u{1b}".to_owned(),
        key: 'c',
        executable: None,
        permissions: PermissionPreset::Standard,
    }];
    assert!(validate_profiles(&profiles).is_err());
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
