//! Closed inventory of user-visible actions and their realistic Rust journey coverage.

use std::{collections::HashSet, fs};

use codex_mux::ui::PUBLIC_ACTION_KINDS;

struct Journey {
    action: &'static str,
    test: &'static str,
    source: &'static str,
}

const JOURNEYS: &[Journey] = &[
    Journey {
        action: "activate",
        test: "packaged_enter_switches_exact_client_cross_session_and_zooms",
        source: include_str!("packaged_runtime_e2e.rs"),
    },
    Journey {
        action: "new",
        test: "packaged_new_resume_fallback_and_confirmed_close_cross_process_boundaries",
        source: include_str!("packaged_runtime_e2e.rs"),
    },
    Journey {
        action: "launch-profile",
        test: "installed_leader_a_popup_launches_a_selected_profile",
        source: include_str!("packaged_runtime_e2e.rs"),
    },
    Journey {
        action: "persist-profiles",
        test: "packaged_profile_edit_persists_through_the_real_popup_flow",
        source: include_str!("packaged_runtime_e2e.rs"),
    },
    Journey {
        action: "smart-naming-toggle",
        test: "persisted_disable_force_stops_a_blocked_owned_daemon_and_its_provider_only",
        source: include_str!("smart_naming_daemon.rs"),
    },
    Journey {
        action: "resume",
        test: "packaged_new_resume_fallback_and_confirmed_close_cross_process_boundaries",
        source: include_str!("packaged_runtime_e2e.rs"),
    },
    Journey {
        action: "close",
        test: "packaged_new_resume_fallback_and_confirmed_close_cross_process_boundaries",
        source: include_str!("packaged_runtime_e2e.rs"),
    },
    Journey {
        action: "rename",
        test: "packaged_popup_renames_an_outside_started_codex_session",
        source: include_str!("packaged_runtime_e2e.rs"),
    },
    Journey {
        action: "unpin",
        test: "packaged_popup_renames_an_outside_started_codex_session",
        source: include_str!("packaged_runtime_e2e.rs"),
    },
    Journey {
        action: "force-auto-name",
        test: "forced_auto_name_progresses_to_success_in_real_tmux",
        source: include_str!("tmux_e2e.rs"),
    },
    Journey {
        action: "theme",
        test: "picker_live_previews_every_theme_and_enter_persists_atomically_then_reloads",
        source: include_str!("packaged_theme_e2e.rs"),
    },
    Journey {
        action: "quit",
        test: "packaged_binary_renders_server_wide_rows_rebuilds_and_handles_navigation_sizes",
        source: include_str!("packaged_runtime_e2e.rs"),
    },
];

#[test]
fn every_public_action_has_a_unique_realistic_rust_journey() {
    let actions = JOURNEYS
        .iter()
        .map(|journey| journey.action)
        .collect::<HashSet<_>>();
    assert_eq!(
        actions.len(),
        JOURNEYS.len(),
        "duplicate action in journey catalog"
    );
    assert_eq!(actions, PUBLIC_ACTION_KINDS.iter().copied().collect());
    for journey in JOURNEYS {
        assert!(
            journey.source.contains(&format!("fn {}", journey.test)),
            "{} maps to missing Rust journey {}",
            journey.action,
            journey.test,
        );
    }

    if std::env::var("CODEX_MUX_VALIDATE_JOURNEY_EVIDENCE").as_deref() == Ok("1") {
        let path = std::env::var_os("CODEX_MUX_JOURNEY_EVIDENCE")
            .expect("CODEX_MUX_JOURNEY_EVIDENCE is required when validation is enabled");
        let evidence = fs::read_to_string(path).expect("read required journey evidence");
        let executed = evidence.lines().collect::<HashSet<_>>();
        assert_eq!(
            executed,
            PUBLIC_ACTION_KINDS.iter().copied().collect(),
            "release journey evidence does not cover the production action inventory"
        );
    }
}
