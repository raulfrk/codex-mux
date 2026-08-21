//! Codex launch command construction.

use std::{ffi::OsString, path::Path};

use crate::domain::{CodexExecutable, InvocationContext, Pane};

/// Codex configuration override that exposes the supported thread identifier
/// through the terminal title.
pub const TERMINAL_TITLE_CONFIG: &str = "tui.terminal_title=[\"thread-id\"]";

/// Interactive Codex launch selected by the user.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LaunchKind {
    /// Start a fresh Codex session.
    New,
    /// Open the Codex session picker without restricting it to the current directory.
    ResumeAll,
}

/// Returns the selected pane directory, falling back to the invoking pane.
#[must_use]
pub fn launch_directory<'a>(
    selected: Option<&'a Pane>,
    context: &'a InvocationContext,
) -> &'a Path {
    selected
        .map(|pane| pane.current_path.as_path())
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(context.current_path.as_path())
}

/// Builds `tmux new-window` arguments without passing Codex through a shell.
///
/// tmux 3.2 and newer execute a multi-argument `shell-command` directly. The
/// executable, configuration override, and resume arguments therefore remain
/// distinct values even when paths contain shell metacharacters.
#[must_use]
pub fn new_window_arguments(
    executable: &CodexExecutable,
    context: &InvocationContext,
    selected: Option<&Pane>,
    kind: LaunchKind,
) -> Vec<OsString> {
    new_window_arguments_with_permissions(executable, context, selected, kind, false)
}

/// Builds direct tmux arguments for a profile-selected Codex launch.
#[must_use]
pub fn new_window_arguments_with_permissions(
    executable: &CodexExecutable,
    context: &InvocationContext,
    selected: Option<&Pane>,
    kind: LaunchKind,
    yolo: bool,
) -> Vec<OsString> {
    let mut arguments = vec![
        OsString::from("new-window"),
        OsString::from("-d"),
        OsString::from("-a"),
        OsString::from("-P"),
        OsString::from("-F"),
        OsString::from("#{pane_id}"),
        OsString::from("-t"),
        OsString::from(context.window_id.as_str()),
        OsString::from("-c"),
        launch_directory(selected, context).as_os_str().to_owned(),
        OsString::from("--"),
        executable.as_path().as_os_str().to_owned(),
        OsString::from("-c"),
        OsString::from(TERMINAL_TITLE_CONFIG),
    ];

    if yolo {
        arguments.push(OsString::from("--yolo"));
    }
    if kind == LaunchKind::ResumeAll {
        arguments.extend([OsString::from("resume"), OsString::from("--all")]);
    }

    arguments
}
