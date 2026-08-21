//! Exact-target tmux actions used by the interactive application.

use std::{
    ffi::OsString,
    str,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{
    MuxError, Result,
    domain::{CodexExecutable, CommandOutput, InvocationContext, Pane, PaneId, TmuxCommandRunner},
    launch::{LaunchKind, new_window_arguments, new_window_arguments_with_permissions},
    tmux::owned_names::{
        IMMEDIATE_NAMING_OPTION, MANUAL_NAME_OPTION, MANUAL_NAME_PID_OPTION,
        MANUAL_NAME_SESSION_OPTION, MANUAL_NAME_SOURCE_OPTION, RENAME_COMPLETE_OPTION,
        UNPIN_COMPLETE_OPTION, UNPIN_READY_OPTION, UNPIN_WAITING_OPTION, UNPIN_WAITING_PID_OPTION,
        UNPIN_WAITING_SESSION_OPTION, UNPIN_WAITING_TITLE_OPTION, clear_marker_arguments,
    },
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
        self.switch_created_pane(context, &pane_id)?;
        Ok(pane_id)
    }

    /// Opens `codex resume --all` in a new window and selects it for the invoking client.
    pub fn resume_all(
        &self,
        context: &InvocationContext,
        selected: Option<&Pane>,
    ) -> Result<PaneId> {
        let pane_id = self.launch(context, selected, LaunchKind::ResumeAll)?;
        self.mark_for_immediate_naming(&pane_id);
        Ok(pane_id)
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
        self.switch_created_pane(context, &pane_id)?;
        self.mark_for_immediate_naming(&pane_id);
        Ok(pane_id)
    }

    /// Kills exactly `pane`; callers are responsible for obtaining UI confirmation first.
    pub fn close_pane(&self, pane: &Pane) -> Result<()> {
        self.run_checked(&os_strings(["kill-pane", "-t", pane.id.as_str()]))?;
        Ok(())
    }

    /// Sets an exact pane title and permanently relinquishes Smart Naming ownership of it.
    ///
    /// Generated metadata is removed and a durable manual-ownership marker is set before
    /// the title changes, in the same tmux command invocation. The marker is deliberately
    /// set first: a partial tmux failure may leave an unchanged title opted out, but it can
    /// never leave a saved title eligible for a later generated-name overwrite.
    pub fn rename_pane(&self, pane: &Pane, title: &str) -> Result<()> {
        let source = if pane.manual_name {
            pane.manual_name_source.as_deref().filter(|value| {
                crate::smart_naming::thread_hint(value).is_some()
                    && pane.manual_name_pid == Some(pane.pane_pid)
                    && pane.manual_name_session.as_ref() == Some(&pane.session_id)
            })
        } else {
            pane.title
                .as_deref()
                .filter(|value| crate::smart_naming::thread_hint(value).is_some())
        };
        let mut arguments = clear_marker_arguments(pane.id.as_str());
        for option in [
            MANUAL_NAME_SOURCE_OPTION,
            MANUAL_NAME_PID_OPTION,
            MANUAL_NAME_SESSION_OPTION,
            UNPIN_READY_OPTION,
            UNPIN_WAITING_OPTION,
            UNPIN_WAITING_TITLE_OPTION,
            UNPIN_WAITING_PID_OPTION,
            UNPIN_WAITING_SESSION_OPTION,
            RENAME_COMPLETE_OPTION,
        ] {
            arguments.push(OsString::from(";"));
            arguments.extend(os_strings([
                "set-option",
                "-pu",
                "-t",
                pane.id.as_str(),
                option,
            ]));
        }
        arguments.push(OsString::from(";"));
        arguments.extend(os_strings([
            "set-option",
            "-p",
            "-t",
            pane.id.as_str(),
            MANUAL_NAME_OPTION,
            "1",
        ]));
        if let Some(source) = source {
            let source_mutation = retained_source_mutation(pane, source);
            if pane.manual_name {
                arguments.push(OsString::from(";"));
                arguments.extend(retained_source_arguments(pane, source));
            } else {
                // An external Codex instance can redraw its terminal title while the
                // popup is open. Keep the manual rename, but retain an unpin source
                // only when that live title still proves the original thread.
                let source_condition =
                    format!("#{{==:#{{pane_title}},{}}}", tmux_format_literal(source));
                arguments.push(OsString::from(";"));
                arguments.extend(os_strings([
                    "if-shell",
                    "-F",
                    "-t",
                    pane.id.as_str(),
                    &source_condition,
                    &source_mutation,
                ]));
            }
        }
        arguments.push(OsString::from(";"));
        arguments.extend(os_strings(["select-pane", "-t", pane.id.as_str(), "-T"]));
        arguments.push(OsString::from(tmux_title_literal(title)));
        let title_index = arguments.len() - 1;
        let token = operation_token();
        arguments.push(OsString::from(";"));
        arguments.extend(os_strings([
            "set-option",
            "-p",
            "-t",
            pane.id.as_str(),
            RENAME_COMPLETE_OPTION,
        ]));
        arguments.push(OsString::from(&token));
        let condition = rename_condition(pane);
        let mutation = tmux_command(&arguments, Some(title_index));
        self.run_checked(&os_strings([
            "if-shell",
            "-F",
            "-t",
            pane.id.as_str(),
            &condition,
            &mutation,
        ]))?;
        self.require_marker(
            pane,
            RENAME_COMPLETE_OPTION,
            &token,
            "pane changed before it could be renamed",
        )?;
        self.run_checked(&os_strings([
            "set-option",
            "-pu",
            "-t",
            pane.id.as_str(),
            RENAME_COMPLETE_OPTION,
        ]))?;
        Ok(())
    }

    /// Restores the retained Codex thread title and makes the pane immediately nameable.
    pub fn unpin_pane(&self, pane: &Pane) -> Result<()> {
        let source = pane.manual_name_source.as_deref().filter(|source| {
            crate::smart_naming::thread_hint(source).is_some()
                && pane.manual_name_pid == Some(pane.pane_pid)
                && pane.manual_name_session.as_ref() == Some(&pane.session_id)
        });
        let Some(source) = source else {
            return self.unpin_without_source(pane);
        };
        let title = pane.title.as_deref().unwrap_or_default();
        let token = operation_token();
        let condition = unpin_condition(pane, source, title, None);
        let restore = format!(
            "select-pane -t {} -T {}; set-option -p -t {} {} {}",
            tmux_quote(pane.id.as_str()),
            tmux_quote(&tmux_title_literal(source)),
            tmux_quote(pane.id.as_str()),
            UNPIN_READY_OPTION,
            tmux_quote(&token),
        );
        self.run_checked(&os_strings([
            "if-shell",
            "-F",
            "-t",
            pane.id.as_str(),
            &condition,
            &restore,
        ]))?;
        self.require_marker(
            pane,
            UNPIN_READY_OPTION,
            &token,
            "pane changed before its manual name could be unpinned",
        )?;
        let condition = unpin_condition(pane, source, source, Some(&token));
        let clear = format!(
            "set-option -pu -t {p} {manual}; set-option -pu -t {p} {source_option}; set-option -pu -t {p} {pid_option}; set-option -pu -t {p} {session_option}; set-option -pu -t {p} {ready}; set-option -p -t {p} {immediate} 1; set-option -p -t {p} {complete} {token}",
            p = tmux_quote(pane.id.as_str()),
            manual = MANUAL_NAME_OPTION,
            source_option = MANUAL_NAME_SOURCE_OPTION,
            pid_option = MANUAL_NAME_PID_OPTION,
            session_option = MANUAL_NAME_SESSION_OPTION,
            ready = UNPIN_READY_OPTION,
            immediate = IMMEDIATE_NAMING_OPTION,
            complete = UNPIN_COMPLETE_OPTION,
            token = tmux_quote(&token),
        );
        self.run_checked(&os_strings([
            "if-shell",
            "-F",
            "-t",
            pane.id.as_str(),
            &condition,
            &clear,
        ]))?;
        self.require_marker(
            pane,
            UNPIN_COMPLETE_OPTION,
            &token,
            "pane changed while its manual name was being unpinned",
        )?;
        self.run_checked(&os_strings([
            "set-option",
            "-pu",
            "-t",
            pane.id.as_str(),
            UNPIN_COMPLETE_OPTION,
        ]))?;
        Ok(())
    }

    /// Relinquishes a manual title even when no conversation identity survived the pin.
    fn unpin_without_source(&self, pane: &Pane) -> Result<()> {
        let title = pane.title.as_deref().unwrap_or_default();
        let token = operation_token();
        let condition = manual_release_condition(pane, title);
        let release = format!(
            "set-option -p -t {p} {waiting} 1; set-option -p -t {p} {waiting_title} {title}; set-option -p -t {p} {waiting_pid} {pid}; set-option -p -t {p} {waiting_session} {session}; set-option -pu -t {p} {manual}; set-option -pu -t {p} {source}; set-option -pu -t {p} {source_pid}; set-option -pu -t {p} {source_session}; set-option -p -t {p} {immediate} 1; set-option -p -t {p} {complete} {token}",
            p = tmux_quote(pane.id.as_str()),
            waiting = UNPIN_WAITING_OPTION,
            waiting_title = UNPIN_WAITING_TITLE_OPTION,
            title = tmux_quote(title),
            waiting_pid = UNPIN_WAITING_PID_OPTION,
            pid = pane.pane_pid,
            waiting_session = UNPIN_WAITING_SESSION_OPTION,
            session = tmux_quote(pane.session_id.as_str()),
            manual = MANUAL_NAME_OPTION,
            source = MANUAL_NAME_SOURCE_OPTION,
            source_pid = MANUAL_NAME_PID_OPTION,
            source_session = MANUAL_NAME_SESSION_OPTION,
            immediate = IMMEDIATE_NAMING_OPTION,
            complete = UNPIN_COMPLETE_OPTION,
            token = tmux_quote(&token),
        );
        self.run_checked(&os_strings([
            "if-shell",
            "-F",
            "-t",
            pane.id.as_str(),
            &condition,
            &release,
        ]))?;
        self.require_marker(
            pane,
            UNPIN_COMPLETE_OPTION,
            &token,
            "pane changed before its manual name could be unpinned",
        )?;
        self.run_checked(&os_strings([
            "set-option",
            "-pu",
            "-t",
            pane.id.as_str(),
            UNPIN_COMPLETE_OPTION,
        ]))?;
        Ok(())
    }

    fn require_marker(
        &self,
        pane: &Pane,
        option: &str,
        expected: &str,
        message: &str,
    ) -> Result<()> {
        let output = self.run_checked(&os_strings([
            "show-options",
            "-pv",
            "-t",
            pane.id.as_str(),
            option,
        ]))?;
        if String::from_utf8_lossy(&output.stdout).trim() == expected {
            Ok(())
        } else {
            Err(MuxError::Command(message.to_owned()))
        }
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
        self.switch_created_pane(context, &pane_id)?;
        Ok(pane_id)
    }

    fn mark_for_immediate_naming(&self, pane_id: &PaneId) {
        // Resume itself has succeeded by this point. Marker failure is harmless:
        // normal immediate discovery still names the pane once it is visible.
        let _ = self.run_checked(&os_strings([
            "set-option",
            "-p",
            "-t",
            pane_id.as_str(),
            IMMEDIATE_NAMING_OPTION,
            "1",
        ]));
    }

    fn switch_created_pane(&self, context: &InvocationContext, pane_id: &PaneId) -> Result<()> {
        self.switch_client(context, pane_id, false)
            .map_err(|error| MuxError::CreatedPaneNotSelected {
                pane: pane_id.to_string(),
                detail: error.to_string(),
            })
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

/// Encodes a title for tmux's `select-pane -T` format parser without changing its text.
///
/// `#{...}`, `#(...)`, plain `#`, and `##` are expanded or collapsed by that parser and
/// therefore require doubled introducers. In contrast, tmux stores any contiguous hash
/// run immediately before `[` literally for pane titles, so that complete run is retained.
fn tmux_title_literal(title: &str) -> String {
    let mut literal = String::with_capacity(title.len());
    let mut characters = title.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '#' {
            literal.push(character);
            continue;
        }

        let mut hashes = 1;
        while characters.next_if_eq(&'#').is_some() {
            hashes += 1;
        }
        if characters.peek().is_some_and(|next| *next == '[') {
            literal.extend(std::iter::repeat_n('#', hashes));
        } else {
            literal.extend(std::iter::repeat_n('#', hashes * 2));
        }
    }
    literal
}

fn retained_source_mutation(pane: &Pane, source: &str) -> String {
    format!(
        "set-option -p -t {pane} {source_option} {source}; set-option -p -t {pane} {pid_option} {pid}; set-option -p -t {pane} {session_option} {session}",
        pane = tmux_quote(pane.id.as_str()),
        source_option = MANUAL_NAME_SOURCE_OPTION,
        source = tmux_quote(source),
        pid_option = MANUAL_NAME_PID_OPTION,
        pid = pane.pane_pid,
        session_option = MANUAL_NAME_SESSION_OPTION,
        session = tmux_quote(pane.session_id.as_str()),
    )
}

fn retained_source_arguments(pane: &Pane, source: &str) -> Vec<OsString> {
    let mut arguments = os_strings([
        "set-option",
        "-p",
        "-t",
        pane.id.as_str(),
        MANUAL_NAME_SOURCE_OPTION,
    ]);
    arguments.push(OsString::from(source));
    arguments.push(OsString::from(";"));
    arguments.extend(os_strings([
        "set-option",
        "-p",
        "-t",
        pane.id.as_str(),
        MANUAL_NAME_PID_OPTION,
        &pane.pane_pid.to_string(),
        ";",
        "set-option",
        "-p",
        "-t",
        pane.id.as_str(),
        MANUAL_NAME_SESSION_OPTION,
        pane.session_id.as_str(),
    ]));
    arguments
}

fn unpin_condition(
    pane: &Pane,
    source: &str,
    expected_title: &str,
    ready_token: Option<&str>,
) -> String {
    let mut clauses = vec![
        format!("#{{==:#{{{MANUAL_NAME_OPTION}}},1}}"),
        format!(
            "#{{==:#{{{MANUAL_NAME_SOURCE_OPTION}}},{}}}",
            tmux_format_literal(source)
        ),
        format!("#{{==:#{{{MANUAL_NAME_PID_OPTION}}},{}}}", pane.pane_pid),
        format!(
            "#{{==:#{{{MANUAL_NAME_SESSION_OPTION}}},{}}}",
            tmux_format_literal(pane.session_id.as_str())
        ),
        format!("#{{==:#{{pane_pid}},{}}}", pane.pane_pid),
        format!(
            "#{{==:#{{session_id}},{}}}",
            tmux_format_literal(pane.session_id.as_str())
        ),
        format!(
            "#{{==:#{{pane_title}},{}}}",
            tmux_format_literal(expected_title)
        ),
    ];
    if let Some(token) = ready_token {
        clauses.push(format!(
            "#{{==:#{{{UNPIN_READY_OPTION}}},{}}}",
            tmux_format_literal(token)
        ));
    }
    clauses
        .into_iter()
        .reduce(|left, right| format!("#{{&&:{left},{right}}}"))
        .unwrap()
}

fn manual_release_condition(pane: &Pane, expected_title: &str) -> String {
    let source = pane.manual_name_source.as_deref().unwrap_or_default();
    [
        format!("#{{==:#{{{MANUAL_NAME_OPTION}}},1}}"),
        format!(
            "#{{==:#{{{MANUAL_NAME_SOURCE_OPTION}}},{}}}",
            tmux_format_literal(source)
        ),
        format!(
            "#{{==:#{{{MANUAL_NAME_PID_OPTION}}},{}}}",
            tmux_format_literal(&pane.manual_name_pid_raw)
        ),
        format!(
            "#{{==:#{{{MANUAL_NAME_SESSION_OPTION}}},{}}}",
            tmux_format_literal(&pane.manual_name_session_raw)
        ),
        format!("#{{==:#{{pane_pid}},{}}}", pane.pane_pid),
        format!(
            "#{{==:#{{session_id}},{}}}",
            tmux_format_literal(pane.session_id.as_str())
        ),
        format!(
            "#{{==:#{{pane_title}},{}}}",
            tmux_format_literal(expected_title)
        ),
    ]
    .into_iter()
    .reduce(|left, right| format!("#{{&&:{left},{right}}}"))
    .unwrap()
}

fn rename_condition(pane: &Pane) -> String {
    let mut clauses = vec![
        format!("#{{==:#{{pane_pid}},{}}}", pane.pane_pid),
        format!(
            "#{{==:#{{session_id}},{}}}",
            tmux_format_literal(pane.session_id.as_str())
        ),
    ];
    if pane.manual_name {
        clauses.push(format!("#{{==:#{{{MANUAL_NAME_OPTION}}},1}}"));
        clauses.push(format!(
            "#{{==:#{{{MANUAL_NAME_SOURCE_OPTION}}},{}}}",
            pane.manual_name_source
                .as_deref()
                .map(tmux_format_literal)
                .unwrap_or_default()
        ));
        clauses.push(format!(
            "#{{==:#{{{MANUAL_NAME_PID_OPTION}}},{}}}",
            pane.manual_name_pid
                .map(|pid| pid.to_string())
                .unwrap_or_default()
        ));
        clauses.push(format!(
            "#{{==:#{{{MANUAL_NAME_SESSION_OPTION}}},{}}}",
            pane.manual_name_session
                .as_ref()
                .map(|session| tmux_format_literal(session.as_str()))
                .unwrap_or_default()
        ));
    } else {
        clauses.push(format!("#{{==:#{{{MANUAL_NAME_OPTION}}},}}"));
    }
    clauses
        .into_iter()
        .reduce(|left, right| format!("#{{&&:{left},{right}}}"))
        .unwrap()
}

fn tmux_command(arguments: &[OsString], literal_index: Option<usize>) -> String {
    arguments
        .iter()
        .enumerate()
        .map(|(index, argument)| {
            if argument == ";" && literal_index != Some(index) {
                ";".to_owned()
            } else {
                tmux_quote(argument.to_string_lossy().as_ref())
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn operation_token() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "op-{}-{nanos}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

fn tmux_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn tmux_format_literal(value: &str) -> String {
    value
        .replace('#', "##")
        .replace(',', "#,")
        .replace('}', "#}")
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
