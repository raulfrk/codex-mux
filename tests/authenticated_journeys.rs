use std::{env, fs, path::PathBuf, process::Command};

use sha2::{Digest, Sha256};

#[allow(dead_code)]
mod support;
use support::{
    PtyProcess, Scratch, TmuxServer, assert_success, serial_tmux_test, tools_available,
    wait_for_file_text_after,
};

fn required_path(name: &str) -> PathBuf {
    PathBuf::from(env::var_os(name).unwrap_or_else(|| panic!("{name} must be set")))
}

fn verified_candidate() -> (PathBuf, PathBuf, String) {
    assert_eq!(
        env::var("CODEX_MUX_RUN_AUTHENTICATED_JOURNEYS").as_deref(),
        Ok("1")
    );
    let candidate = required_path("CODEX_MUX_CANDIDATE_BINARY");
    let codex = required_path("CODEX_MUX_AUTHENTICATED_CODEX");
    let expected_digest = env::var("CODEX_MUX_CANDIDATE_SHA256")
        .expect("CODEX_MUX_CANDIDATE_SHA256 must bind the approved candidate");
    let digest = format!(
        "{:x}",
        Sha256::digest(fs::read(&candidate).expect("read candidate"))
    );
    assert_eq!(digest, expected_digest, "candidate artifact digest changed");
    (candidate, codex, digest)
}

#[test]
#[ignore = "requires authenticated Codex, tmux, and the exact release-candidate artifact"]
fn authenticated_candidate_multiplexes_real_luna_naming_and_records_artifact_identity() {
    let (candidate, codex, digest) = verified_candidate();
    let version = Command::new(&candidate)
        .arg("--version")
        .output()
        .expect("run candidate --version");
    assert!(
        version.status.success(),
        "candidate artifact is not runnable"
    );
    assert_eq!(
        String::from_utf8_lossy(&version.stdout).trim(),
        format!("codex-mux {}", env!("CARGO_PKG_VERSION")),
        "candidate version does not match the test package"
    );
    let naming = Command::new(&candidate)
        .env("CODEX_MUX_RUN_AUTHENTICATED_JOURNEYS", "1")
        .arg("--launch-executable")
        .arg(&codex)
        .arg("--match-executable")
        .arg(&codex)
        .arg("--pane-command")
        .arg("codex")
        .arg("authenticated-naming-journey")
        .output()
        .expect("run candidate authenticated naming journey");
    assert_success(&naming, "candidate authenticated naming journey");
    assert_eq!(
        String::from_utf8_lossy(&naming.stdout).trim(),
        "authenticated-naming-journey titles=2"
    );

    let commit = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("read candidate commit");
    assert!(commit.status.success());
    eprintln!(
        "authenticated-release-evidence commit={} sha256={} version={} titles={}",
        String::from_utf8_lossy(&commit.stdout).trim(),
        digest,
        String::from_utf8_lossy(&version.stdout).trim(),
        2,
    );
}

#[test]
#[ignore = "requires authenticated interactive Codex max/xhigh and the exact candidate artifact"]
fn authenticated_real_codex_max_ultra_opens_both_mux_entry_paths_repeatedly() {
    let _serial = serial_tmux_test();
    assert!(tools_available(), "tmux and script are required");
    let (candidate, codex, digest) = verified_candidate();
    let scratch = Scratch::new("authenticated-max-ultra-entry");
    let config = scratch.join("tmux.conf");
    fs::write(
        &config,
        "set -g status off\nset -g prefix C-b\nset -g default-shell /bin/sh\n",
    )
    .unwrap();
    let server = TmuxServer::start(&config, "origin", scratch.path());
    let install = Command::new(&candidate)
        .env("TMUX", server.tmux_environment())
        .args(["--launch-executable"])
        .arg(&codex)
        .arg("--match-executable")
        .arg(&codex)
        .args([
            "--pane-command",
            "codex",
            "tmux",
            "install",
            "--smart-left",
            "--config",
        ])
        .arg(&config)
        .output()
        .expect("install candidate bindings");
    assert_success(&install, "install candidate bindings");
    let pane = server
        .checked(&["display-message", "-p", "-t", "origin", "#{pane_id}"])
        .trim()
        .to_owned();
    let capture = scratch.join("client.typescript");
    let mut client = PtyProcess::attach_captured(&server, "origin", 120, 40, &capture);
    let tty = server
        .checked(&["display-message", "-p", "-t", &pane, "#{client_tty}"])
        .trim()
        .to_owned();

    for effort in ["medium", "max", "xhigh"] {
        let respawn_offset = fs::metadata(&capture).map_or(0, |metadata| metadata.len()) as usize;
        let effort_config = format!("model_reasoning_effort=\"{effort}\"");
        let output = server
            .command()
            .args(["respawn-pane", "-k", "-t", &pane, "--"])
            .arg(&codex)
            .args(["--no-alt-screen", "-c", &effort_config])
            .output()
            .expect("start authenticated Codex reasoning mode");
        assert_success(&output, "start authenticated Codex reasoning mode");
        server.wait_until("real Codex foreground", || {
            server
                .checked(&[
                    "display-message",
                    "-p",
                    "-t",
                    &pane,
                    "#{pane_current_command}",
                ])
                .trim()
                == "codex"
        });
        wait_for_file_text_after(&capture, respawn_offset, "OpenAI Codex");

        for entry in [b"\x1b[D".as_slice(), b"\x02a".as_slice()] {
            let offset = fs::metadata(&capture).map_or(0, |metadata| metadata.len()) as usize;
            client.send(entry);
            let bytes = wait_for_file_text_after(&capture, offset, "\x1b[6;12H┌");
            let appended = String::from_utf8_lossy(bytes.get(offset..).unwrap_or_default());
            assert!(
                appended.contains("\x1b[6;12H┌"),
                "{effort} popup did not render as a centered 96x28 popup at 120x40"
            );
            wait_for_file_text_after(&capture, offset, "codex-mux");
            client.send(b"q");
            server.wait_until("mux popup closes", || {
                let guard = server.run(&[
                    "show-options",
                    "-pqv",
                    "-t",
                    &pane,
                    "@codex_mux_smart_left_active",
                ]);
                let popup = server.run(&["display-message", "-p", "-c", &tty, "#{popup_active}"]);
                guard.status.success()
                    && guard.stdout.is_empty()
                    && popup.status.success()
                    && String::from_utf8_lossy(&popup.stdout).trim() != "1"
            });
        }
    }
    client.send(b"\x02d");
    eprintln!("authenticated-max-ultra-entry-evidence sha256={digest}");
}
