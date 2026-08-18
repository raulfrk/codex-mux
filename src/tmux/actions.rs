//! Exact-target tmux actions used by the interactive application.

use std::{ffi::OsString, str};

use crate::{
    MuxError, Result,
    domain::{CodexExecutable, CommandOutput, InvocationContext, Pane, PaneId, TmuxCommandRunner},
    launch::{LaunchKind, new_window_arguments, new_window_arguments_with_permissions},
};

/// Executes interactive actions through an injectable tmux command boundary.
pub struct TmuxActions<'a, Runner> {
    runner: &'a Runner,
    executable: &'a CodexExecutable,
}

impl<'a, Runner> TmuxActions<'a, Runner>
where
    Runner: TmuxCommandRunner,
{
    /// Creates an action service for one configured Codex executable.
    #[must_use]
    pub const fn new(runner: &'a Runner, executable: &'a CodexExecutable) -> Self {
        Self { runner, executable }
    }

    /// Switches the invoking client to `pane` and leaves its window zoomed.
    ///
    /// tmux stores active-window and zoom state on the session/window, so clients
    /// already attached to the target session observe those shared state changes.
    /// Clients attached to other sessions are not switched.
    pub fn switch_and_zoom(&self, context: &InvocationContext, pane: &Pane) -> Result<()> {
        let zoom_query = os_strings([
            "display-message",
            "-p",
            "-t",
            pane.id.as_str(),
            "#{window_zoomed_flag}",
        ]);
        let output = self.run_checked(&zoom_query)?;
        let was_zoomed = parse_zoomed(&output.stdout)?;

        self.switch_client(context, &pane.id, !was_zoomed)?;

        Ok(())
    }

    /// Starts a fresh Codex session in a new window and selects it for the invoking client.
    pub fn new_session(
        &self,
        context: &InvocationContext,
        selected: Option<&Pane>,
    ) -> Result<PaneId> {
        self.launch(context, selected, LaunchKind::New)
    }

    /// Starts a fresh session with a profile-selected executable and permissions.
    pub fn new_session_with_profile(
        &self,
        context: &InvocationContext,
        selected: Option<&Pane>,
        executable: &CodexExecutable,
        yolo: bool,
    ) -> Result<PaneId> {
        let arguments = new_window_arguments_with_permissions(
            executable,
            context,
            selected,
            LaunchKind::New,
            yolo,
        );
        let output = self.run_checked(&arguments)?;
        let pane_id = parse_pane_id(&output.stdout)?;
        self.switch_client(context, &pane_id, false)?;
        Ok(pane_id)
    }

    /// Opens `codex resume --all` in a new window and selects it for the invoking client.
    pub fn resume_all(
        &self,
        context: &InvocationContext,
        selected: Option<&Pane>,
    ) -> Result<PaneId> {
        self.launch(context, selected, LaunchKind::ResumeAll)
    }

    /// Opens `codex resume --all` with a profile-selected executable and permissions.
    pub fn resume_all_with_profile(
        &self,
        context: &InvocationContext,
        selected: Option<&Pane>,
        executable: &CodexExecutable,
        yolo: bool,
    ) -> Result<PaneId> {
        let arguments = new_window_arguments_with_permissions(
            executable,
            context,
            selected,
            LaunchKind::ResumeAll,
            yolo,
        );
        let output = self.run_checked(&arguments)?;
        let pane_id = parse_pane_id(&output.stdout)?;
        self.switch_client(context, &pane_id, false)?;
        Ok(pane_id)
    }

    /// Kills exactly `pane`; callers are responsible for obtaining UI confirmation first.
    pub fn close_pane(&self, pane: &Pane) -> Result<()> {
        self.run_checked(&os_strings(["kill-pane", "-t", pane.id.as_str()]))?;
        Ok(())
    }

    fn launch(
        &self,
        context: &InvocationContext,
        selected: Option<&Pane>,
        kind: LaunchKind,
    ) -> Result<PaneId> {
        let arguments = new_window_arguments(self.executable, context, selected, kind);
        let output = self.run_checked(&arguments)?;
        let pane_id = parse_pane_id(&output.stdout)?;
        self.switch_client(context, &pane_id, false)?;
        Ok(pane_id)
    }

    fn switch_client(
        &self,
        context: &InvocationContext,
        pane_id: &PaneId,
        zoom_if_unzoomed: bool,
    ) -> Result<()> {
        let mut arguments = os_strings(["select-window", "-t", pane_id.as_str()]);
        arguments.push(OsString::from(";"));
        arguments.extend(os_strings(["select-pane", "-Z", "-t", pane_id.as_str()]));
        arguments.push(OsString::from(";"));
        arguments.extend(os_strings([
            "switch-client",
            "-Z",
            "-c",
            context.client_id.as_str(),
            "-t",
            pane_id.as_str(),
        ]));
        if zoom_if_unzoomed {
            arguments.push(OsString::from(";"));
            arguments.extend(os_strings(["resize-pane", "-Z", "-t", pane_id.as_str()]));
        }
        self.run_checked(&arguments)?;
        Ok(())
    }

    fn run_checked(&self, arguments: &[OsString]) -> Result<CommandOutput> {
        let output = self.runner.run(arguments)?;
        if output.status == Some(0) {
            return Ok(output);
        }

        let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let detail = if detail.is_empty() {
            match output.status {
                Some(code) => format!("tmux exited with status {code}"),
                None => "tmux terminated without an exit status".to_owned(),
            }
        } else {
            detail
        };
        Err(MuxError::Command(detail))
    }
}

fn parse_zoomed(stdout: &[u8]) -> Result<bool> {
    match stdout {
        b"0\n" | b"0" => Ok(false),
        b"1\n" | b"1" => Ok(true),
        _ => Err(MuxError::Command(format!(
            "tmux returned an invalid zoom flag {:?}",
            String::from_utf8_lossy(stdout).trim()
        ))),
    }
}

fn parse_pane_id(stdout: &[u8]) -> Result<PaneId> {
    let value = str::from_utf8(stdout)
        .map_err(|_| MuxError::Command("tmux returned a non-UTF-8 pane ID".to_owned()))?
        .trim();
    if !value.starts_with('%') || value[1..].parse::<u64>().is_err() {
        return Err(MuxError::Command(format!(
            "tmux returned an invalid pane ID {value:?}"
        )));
    }
    PaneId::new(value)
}

fn os_strings<const N: usize>(values: [&str; N]) -> Vec<OsString> {
    values.into_iter().map(OsString::from).collect()
}
