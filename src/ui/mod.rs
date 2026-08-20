//! Keyboard-first state model and deterministic Ratatui renderer.

pub mod terminal;

use std::cmp;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::Style,
    text::Line,
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::{
    config::{LaunchProfile, PermissionPreset, validate_profiles},
    domain::{Pane, PaneId, TerminalSize, ThemeId},
    theme::{Theme, theme},
};

const MAX_MANUAL_TITLE_CHARS: usize = 80;

/// Responsive rendering profile selected from the terminal dimensions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LayoutKind {
    /// Full list plus persistent command sidebar.
    Wide,
    /// Full-width list with a footer command bar.
    Compact,
    /// Compact two-line rows and abbreviated phone controls.
    Phone,
    /// Minimal selected-session view for severely constrained terminals.
    Tiny,
}

/// Chooses a stable layout without depending on frame history.
#[must_use]
pub const fn layout_kind(size: TerminalSize) -> LayoutKind {
    if size.width < 40 || size.height < 12 {
        LayoutKind::Tiny
    } else if size.width <= 62 || size.height < 20 {
        LayoutKind::Phone
    } else if size.is_compact() {
        LayoutKind::Compact
    } else {
        LayoutKind::Wide
    }
}

/// High-level action requested by a key press.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Action {
    /// Activate the selected tmux pane.
    Activate(PaneId),
    /// Start a new Codex session.
    New,
    /// Start a new session using the selected reusable profile.
    LaunchProfile(LaunchProfile),
    /// Persist a complete replacement profile set.
    PersistProfiles(Vec<LaunchProfile>),
    /// Persist an explicit conversation-aware naming preference.
    PersistSmartNaming(bool),
    /// Start Codex with its resume subcommand.
    Resume,
    /// Close the pane after explicit confirmation.
    Close(PaneId),
    /// Assign a user-owned title to one pane and relinquish Smart Naming ownership.
    Rename(PaneId, String),
    /// Remove manual ownership and resume Smart Naming for the selected pane.
    Unpin(PaneId),
    /// Persist a theme chosen in the live-preview picker.
    PersistTheme(ThemeId),
    /// Leave the popup without changing tmux state.
    Quit,
}

/// Per-invocation policy controlling whether colored themes may be selected.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ColorPolicy {
    /// Allow normal live preview and theme persistence.
    #[default]
    Allow,
    /// Force monochrome and disable theme selection without changing preferences.
    ForceMonochrome,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Mode {
    Browse,
    ConfirmClose(PaneId),
    ManualRename(ManualRename),
    ThemePicker {
        original: ThemeId,
    },
    ProfilePicker {
        selected: usize,
        launch_kind: ProfileLaunchKind,
    },
    ProfileEditor(ProfileEditor),
    Configuration,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProfileLaunchKind {
    New,
    Resume,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EditorField {
    Name,
    Key,
    Executable,
    Permissions,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProfileEditor {
    original: Option<usize>,
    name: String,
    key: String,
    executable: String,
    permissions: PermissionPreset,
    field: EditorField,
    error: Option<String>,
    launch_kind: ProfileLaunchKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ManualRename {
    pane_id: PaneId,
    title: String,
    error: Option<String>,
    untouched: bool,
    can_unpin: bool,
    unpin_unavailable_reason: Option<&'static str>,
}

impl ManualRename {
    fn for_pane(pane: &Pane) -> Self {
        let unpin_unavailable_reason = if !pane.manual_name {
            None
        } else if pane
            .manual_name_source
            .as_deref()
            .is_none_or(|source| crate::smart_naming::thread_hint(source).is_none())
        {
            Some("unpin unavailable: source not retained")
        } else if pane.manual_name_pid != Some(pane.pane_pid)
            || pane.manual_name_session.as_ref() != Some(&pane.session_id)
        {
            Some("unpin unavailable: the pane process or session changed")
        } else {
            None
        };
        Self {
            pane_id: pane.id.clone(),
            title: pane.display_title(),
            error: None,
            untouched: true,
            can_unpin: pane.manual_name && unpin_unavailable_reason.is_none(),
            unpin_unavailable_reason,
        }
    }
}

impl ProfileEditor {
    fn create(launch_kind: ProfileLaunchKind) -> Self {
        Self {
            original: None,
            name: String::new(),
            key: String::new(),
            executable: String::new(),
            permissions: PermissionPreset::Standard,
            field: EditorField::Name,
            error: None,
            launch_kind,
        }
    }

    fn edit(index: usize, profile: &LaunchProfile, launch_kind: ProfileLaunchKind) -> Self {
        Self {
            original: Some(index),
            name: profile.name.clone(),
            key: profile.key.to_string(),
            executable: profile
                .executable
                .as_ref()
                .map_or_else(String::new, |path| path.display().to_string()),
            permissions: profile.permissions,
            field: EditorField::Name,
            error: None,
            launch_kind,
        }
    }
}

/// Complete renderer-independent UI state.
#[derive(Clone, Debug)]
pub struct App {
    panes: Vec<Pane>,
    selected: Option<PaneId>,
    theme: ThemeId,
    color_policy: ColorPolicy,
    mode: Mode,
    warning: Option<String>,
    profiles: Vec<LaunchProfile>,
    resume_profile: Option<LaunchProfile>,
    smart_naming: bool,
    naming_save_warning: bool,
    smart_naming_pending: bool,
    inventory_warning: bool,
}

impl App {
    /// Creates an application with the first pane selected.
    #[must_use]
    pub fn new(panes: Vec<Pane>, theme: ThemeId, warning: Option<String>) -> Self {
        Self::with_color_policy(panes, theme, warning, ColorPolicy::Allow)
    }

    /// Creates an application with an explicit per-invocation color policy.
    #[must_use]
    pub fn with_color_policy(
        panes: Vec<Pane>,
        theme: ThemeId,
        warning: Option<String>,
        color_policy: ColorPolicy,
    ) -> Self {
        Self::with_profiles(
            panes,
            theme,
            warning,
            color_policy,
            vec![LaunchProfile::standard(), LaunchProfile::yolo()],
        )
    }

    /// Creates application state with an explicit persisted profile set.
    #[must_use]
    pub fn with_profiles(
        panes: Vec<Pane>,
        theme: ThemeId,
        warning: Option<String>,
        color_policy: ColorPolicy,
        profiles: Vec<LaunchProfile>,
    ) -> Self {
        Self::with_settings(panes, theme, warning, color_policy, profiles, false)
    }

    /// Creates application state with all persisted user settings.
    #[must_use]
    pub fn with_settings(
        panes: Vec<Pane>,
        theme: ThemeId,
        warning: Option<String>,
        color_policy: ColorPolicy,
        profiles: Vec<LaunchProfile>,
        smart_naming: bool,
    ) -> Self {
        let selected = panes.first().map(|pane| pane.id.clone());
        Self {
            panes,
            selected,
            theme: if color_policy == ColorPolicy::ForceMonochrome {
                ThemeId::Monochrome
            } else {
                theme
            },
            color_policy,
            mode: Mode::Browse,
            warning,
            profiles,
            resume_profile: None,
            smart_naming,
            naming_save_warning: false,
            smart_naming_pending: false,
            inventory_warning: false,
        }
    }

    /// Returns whether conversation-aware naming is currently enabled.
    #[must_use]
    pub const fn smart_naming_enabled(&self) -> bool {
        self.smart_naming
    }

    /// Restores the prior value after persistence fails.
    pub fn smart_naming_save_failed(&mut self, error: impl Into<String>) {
        self.warning = Some(error.into());
        self.inventory_warning = false;
        self.naming_save_warning = true;
        self.smart_naming_pending = false;
    }

    /// Confirms persistence and clears only a prior naming-save warning.
    pub fn smart_naming_saved(&mut self, enabled: bool) {
        self.smart_naming = enabled;
        self.smart_naming_pending = false;
        if self.naming_save_warning {
            self.warning = None;
            self.naming_save_warning = false;
        }
    }

    /// Reports a non-blocking provider startup failure while retaining opt-in.
    pub fn smart_naming_runtime_failed(&mut self, error: impl Into<String>) {
        self.warning = Some(error.into());
        self.inventory_warning = false;
        self.smart_naming_pending = false;
    }

    /// Keeps the prior visible state while a daemon shutdown is acknowledged.
    pub fn smart_naming_stopping(&mut self) {
        self.smart_naming_pending = true;
    }

    /// Publishes one coherent inventory snapshot and clears its prior warning.
    pub fn inventory_refreshed(&mut self, panes: Vec<Pane>) {
        self.replace_panes(panes);
        if self.inventory_warning {
            self.warning = None;
            self.inventory_warning = false;
        }
    }

    /// Reports a refresh failure without blocking input or discarding the last snapshot.
    pub fn inventory_failed(&mut self, error: impl Into<String>) {
        if self.warning.is_none() {
            self.warning = Some(error.into());
            self.inventory_warning = true;
        }
    }

    /// Returns the active launch profiles.
    #[must_use]
    pub fn profiles(&self) -> &[LaunchProfile] {
        &self.profiles
    }

    /// Returns the profile selected for a pending resume action.
    #[must_use]
    pub const fn resume_profile(&self) -> Option<&LaunchProfile> {
        self.resume_profile.as_ref()
    }

    /// Completes a successful profile save and returns to the picker.
    pub fn profiles_saved(&mut self, profiles: Vec<LaunchProfile>) {
        let (selected, launch_kind) = match &self.mode {
            Mode::ProfileEditor(editor) => (
                editor.original.unwrap_or(profiles.len().saturating_sub(1)),
                editor.launch_kind,
            ),
            _ => (0, ProfileLaunchKind::New),
        };
        self.profiles = profiles;
        self.mode = Mode::ProfilePicker {
            selected,
            launch_kind,
        };
    }

    /// Keeps the editor open and displays a persistence or validation failure.
    pub fn profile_save_failed(&mut self, error: impl Into<String>) {
        if let Mode::ProfileEditor(editor) = &mut self.mode {
            editor.error = Some(error.into());
        }
    }

    /// Returns the panes in their display order.
    #[must_use]
    pub fn panes(&self) -> &[Pane] {
        &self.panes
    }

    /// Returns the selected stable pane identity.
    #[must_use]
    pub fn selected_pane_id(&self) -> Option<&PaneId> {
        self.selected.as_ref()
    }

    /// Selects the requested pane when it is present in the current inventory.
    pub fn select_pane(&mut self, id: &PaneId) {
        if self.panes.iter().any(|pane| &pane.id == id) {
            self.selected = Some(id.clone());
        }
    }

    /// Returns the theme currently shown, including picker live previews.
    #[must_use]
    pub const fn active_theme(&self) -> ThemeId {
        self.theme
    }

    /// Replaces discovery results while preserving selection by pane identity.
    pub fn replace_panes(&mut self, panes: Vec<Pane>) {
        let old_index = self.selected_index().unwrap_or(0);
        let old_selection = self.selected.clone();
        self.panes = panes;
        self.selected = old_selection
            .filter(|id| self.panes.iter().any(|pane| &pane.id == id))
            .or_else(|| {
                let index = cmp::min(old_index, self.panes.len().saturating_sub(1));
                self.panes.get(index).map(|pane| pane.id.clone())
            });
        if matches!(&self.mode, Mode::ConfirmClose(id) if !self.panes.iter().any(|pane| &pane.id == id))
        {
            self.mode = Mode::Browse;
        }
    }

    /// Applies a key event and returns an action for the tmux/application layer.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Action> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        match self.mode.clone() {
            Mode::Browse => self.handle_browse_key(key.code),
            Mode::ConfirmClose(id) => self.handle_confirmation_key(key, id),
            Mode::ManualRename(editor) => self.handle_manual_rename_key(key.code, editor),
            Mode::ThemePicker { original } => self.handle_theme_key(key.code, original),
            Mode::ProfilePicker {
                selected,
                launch_kind,
            } => self.handle_profile_key(key.code, selected, launch_kind),
            Mode::ProfileEditor(editor) => self.handle_editor_key(key.code, editor),
            Mode::Configuration => self.handle_configuration_key(key.code),
        }
    }

    fn handle_browse_key(&mut self, code: KeyCode) -> Option<Action> {
        match code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.move_selection(1);
                None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.move_selection(-1);
                None
            }
            KeyCode::Enter => self.selected.clone().map(Action::Activate),
            KeyCode::Char('n') => {
                self.resume_profile = None;
                self.mode = Mode::ProfilePicker {
                    selected: 0,
                    launch_kind: ProfileLaunchKind::New,
                };
                None
            }
            KeyCode::Char('r') => {
                self.resume_profile = None;
                self.mode = Mode::ProfilePicker {
                    selected: 0,
                    launch_kind: ProfileLaunchKind::Resume,
                };
                None
            }
            KeyCode::Char('x') => {
                if let Some(id) = self.selected.clone() {
                    self.mode = Mode::ConfirmClose(id);
                }
                None
            }
            KeyCode::Char('R') => {
                if let Some(pane) = self
                    .selected
                    .as_ref()
                    .and_then(|id| self.panes.iter().find(|pane| &pane.id == id))
                {
                    self.mode = Mode::ManualRename(ManualRename::for_pane(pane));
                }
                None
            }
            KeyCode::Char('t') => {
                if self.color_policy == ColorPolicy::Allow {
                    self.mode = Mode::ThemePicker {
                        original: self.theme,
                    };
                }
                None
            }
            KeyCode::Char('c') | KeyCode::Char('C') => {
                self.mode = Mode::Configuration;
                None
            }
            KeyCode::Char('q') | KeyCode::Esc => Some(Action::Quit),
            _ => None,
        }
    }

    fn handle_configuration_key(&mut self, code: KeyCode) -> Option<Action> {
        match code {
            KeyCode::Char('n') | KeyCode::Char('N') => (!self.smart_naming_pending)
                .then_some(Action::PersistSmartNaming(!self.smart_naming)),
            KeyCode::Char('c') | KeyCode::Char('C') | KeyCode::Esc | KeyCode::Char('q') => {
                self.mode = Mode::Browse;
                None
            }
            _ => None,
        }
    }

    fn handle_confirmation_key(&mut self, key: KeyEvent, id: PaneId) -> Option<Action> {
        if key.kind != KeyEventKind::Press {
            return None;
        }
        match key.code {
            KeyCode::Char('x') | KeyCode::Enter => {
                self.mode = Mode::Browse;
                Some(Action::Close(id))
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                self.mode = Mode::Browse;
                None
            }
            _ => None,
        }
    }

    fn handle_manual_rename_key(
        &mut self,
        code: KeyCode,
        mut editor: ManualRename,
    ) -> Option<Action> {
        editor.error = None;
        match code {
            KeyCode::Esc => {
                self.mode = Mode::Browse;
                return None;
            }
            KeyCode::Backspace => {
                editor.title.pop();
                editor.untouched = false;
            }
            KeyCode::Char('c') if editor.untouched => {
                editor.title.clear();
                editor.untouched = false;
            }
            KeyCode::Char(character) => {
                editor.title.push(character);
                editor.untouched = false;
            }
            KeyCode::Enter if editor.title.trim().is_empty() && editor.can_unpin => {
                self.mode = Mode::Browse;
                return Some(Action::Unpin(editor.pane_id));
            }
            KeyCode::Enter => match validate_manual_title(&editor.title) {
                Ok(title) => {
                    self.mode = Mode::Browse;
                    return Some(Action::Rename(editor.pane_id, title));
                }
                Err(error) => editor.error = Some(error.to_owned()),
            },
            _ => {}
        }
        self.mode = Mode::ManualRename(editor);
        None
    }

    fn handle_theme_key(&mut self, code: KeyCode, original: ThemeId) -> Option<Action> {
        match code {
            KeyCode::Char('j') | KeyCode::Down | KeyCode::Char('t') => {
                self.cycle_theme(1);
                None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.cycle_theme(-1);
                None
            }
            KeyCode::Enter => {
                self.mode = Mode::Browse;
                Some(Action::PersistTheme(self.theme))
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                self.theme = original;
                self.mode = Mode::Browse;
                None
            }
            _ => None,
        }
    }

    fn handle_profile_key(
        &mut self,
        code: KeyCode,
        selected: usize,
        launch_kind: ProfileLaunchKind,
    ) -> Option<Action> {
        match code {
            KeyCode::Char('j') | KeyCode::Down => {
                let next = selected
                    .saturating_add(1)
                    .min(self.profiles.len().saturating_sub(1));
                self.mode = Mode::ProfilePicker {
                    selected: next,
                    launch_kind,
                };
                None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.mode = Mode::ProfilePicker {
                    selected: selected.saturating_sub(1),
                    launch_kind,
                };
                None
            }
            KeyCode::Enter => {
                let profile = self.profiles.get(selected)?.clone();
                Some(self.profile_launch_action(launch_kind, profile))
            }
            KeyCode::Char('a') => {
                self.mode = Mode::ProfileEditor(ProfileEditor::create(launch_kind));
                None
            }
            KeyCode::Char('e') => {
                let profile = self.profiles.get(selected)?.clone();
                self.mode =
                    Mode::ProfileEditor(ProfileEditor::edit(selected, &profile, launch_kind));
                None
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                self.mode = Mode::Browse;
                None
            }
            KeyCode::Char(key) => {
                let profile = self
                    .profiles
                    .iter()
                    .find(|profile| profile.key.eq_ignore_ascii_case(&key))?
                    .clone();
                Some(self.profile_launch_action(launch_kind, profile))
            }
            _ => None,
        }
    }

    fn handle_editor_key(&mut self, code: KeyCode, mut editor: ProfileEditor) -> Option<Action> {
        editor.error = None;
        match code {
            KeyCode::Esc => {
                self.mode = Mode::ProfilePicker {
                    selected: editor.original.unwrap_or(0),
                    launch_kind: editor.launch_kind,
                };
                return None;
            }
            KeyCode::Tab | KeyCode::Down => {
                editor.field = next_editor_field(editor.field, 1);
            }
            KeyCode::BackTab | KeyCode::Up => {
                editor.field = next_editor_field(editor.field, -1);
            }
            KeyCode::Left | KeyCode::Right if editor.field == EditorField::Permissions => {
                editor.permissions = match editor.permissions {
                    PermissionPreset::Standard => PermissionPreset::Yolo,
                    PermissionPreset::Yolo => PermissionPreset::Standard,
                };
            }
            KeyCode::Backspace => match editor.field {
                EditorField::Name => {
                    editor.name.pop();
                }
                EditorField::Key => {
                    editor.key.pop();
                }
                EditorField::Executable => {
                    editor.executable.pop();
                }
                EditorField::Permissions => {}
            },
            KeyCode::Char(character) => match editor.field {
                EditorField::Name => editor.name.push(character),
                EditorField::Key if editor.key.is_empty() => editor.key.push(character),
                EditorField::Executable => editor.executable.push(character),
                EditorField::Key | EditorField::Permissions => {}
            },
            KeyCode::Enter => {
                let key = editor.key.chars().next().unwrap_or('\0');
                let profile = LaunchProfile {
                    name: editor.name.trim().to_owned(),
                    key,
                    executable: (!editor.executable.trim().is_empty())
                        .then(|| std::path::PathBuf::from(editor.executable.trim())),
                    permissions: editor.permissions,
                };
                let mut profiles = self.profiles.clone();
                if let Some(index) = editor.original {
                    profiles[index] = profile;
                } else {
                    profiles.push(profile);
                }
                if let Err(error) = validate_profiles(&profiles) {
                    editor.error = Some(error.to_string());
                } else {
                    self.mode = Mode::ProfileEditor(editor);
                    return Some(Action::PersistProfiles(profiles));
                }
            }
            _ => {}
        }
        self.mode = Mode::ProfileEditor(editor);
        None
    }

    fn selected_index(&self) -> Option<usize> {
        let selected = self.selected.as_ref()?;
        self.panes.iter().position(|pane| &pane.id == selected)
    }

    fn move_selection(&mut self, delta: isize) {
        if self.panes.is_empty() {
            self.selected = None;
            return;
        }
        let current = self.selected_index().unwrap_or(0);
        let last = self.panes.len() - 1;
        let next = if delta.is_negative() {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            current.saturating_add(delta as usize).min(last)
        };
        self.selected = Some(self.panes[next].id.clone());
    }

    fn profile_launch_action(&mut self, kind: ProfileLaunchKind, profile: LaunchProfile) -> Action {
        match kind {
            ProfileLaunchKind::New => Action::LaunchProfile(profile),
            ProfileLaunchKind::Resume => {
                self.resume_profile = Some(profile);
                Action::Resume
            }
        }
    }

    fn cycle_theme(&mut self, delta: isize) {
        let current = ThemeId::ALL
            .iter()
            .position(|candidate| *candidate == self.theme)
            .unwrap_or(0);
        let count = ThemeId::ALL.len() as isize;
        let next = (current as isize + delta).rem_euclid(count) as usize;
        self.theme = ThemeId::ALL[next];
    }
}

/// Renders the current application state into one deterministic frame.
pub fn render(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();
    if area.is_empty() {
        return;
    }
    let palette = theme(app.theme);
    frame.render_widget(Block::default().style(palette.text), area);
    match layout_kind(TerminalSize {
        width: area.width,
        height: area.height,
    }) {
        LayoutKind::Wide => render_wide(frame, area, app, palette),
        LayoutKind::Compact => render_compact(frame, area, app, palette, false),
        LayoutKind::Phone => render_compact(frame, area, app, palette, true),
        LayoutKind::Tiny => render_tiny(frame, area, app, palette),
    }
    if matches!(app.mode, Mode::ThemePicker { .. }) {
        render_theme_picker(frame, area, app, palette);
    }
    match &app.mode {
        Mode::ProfilePicker { selected, .. } => {
            render_profile_picker(frame, area, app, palette, *selected);
        }
        Mode::ProfileEditor(editor) => render_profile_editor(frame, area, editor, palette),
        Mode::ManualRename(editor) => render_manual_rename(frame, area, editor, palette),
        Mode::Configuration => render_configuration(frame, area, app, palette),
        _ => {}
    }
}

fn next_editor_field(field: EditorField, delta: isize) -> EditorField {
    let fields = [
        EditorField::Name,
        EditorField::Key,
        EditorField::Executable,
        EditorField::Permissions,
    ];
    let current = fields
        .iter()
        .position(|candidate| *candidate == field)
        .unwrap_or(0);
    fields[(current as isize + delta).rem_euclid(fields.len() as isize) as usize]
}

fn render_wide(frame: &mut Frame<'_>, area: Rect, app: &App, palette: Theme) {
    let outer = Block::default()
        .title(" codex-mux ")
        .borders(Borders::ALL)
        .border_style(palette.accent);
    let inner = outer.inner(area);
    frame.render_widget(outer, area);
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(45), Constraint::Length(29)])
        .split(inner);
    render_list(frame, columns[0], app, palette);
    let mut help_lines = vec![
        Line::styled("Commands", palette.accent),
        Line::from("Enter  switch"),
        Line::from("n      new session"),
        Line::from("r      resume"),
        Line::from("R      rename"),
        Line::from("x x    close"),
        Line::from("t      themes"),
        Line::from("c      config"),
        Line::from("q/Esc  quit"),
    ];
    if matches!(app.mode, Mode::ConfirmClose(_)) {
        help_lines.push(Line::default());
        help_lines.push(Line::styled("Close selected pane?", palette.warning));
        help_lines.push(Line::styled("x/Enter yes · Esc no", palette.warning));
    } else if let Some(warning) = &app.warning {
        help_lines.push(Line::default());
        help_lines.push(Line::styled("Preference warning", palette.warning));
        help_lines.push(Line::styled(sanitized(warning), palette.warning));
    }
    let help = Paragraph::new(help_lines)
        .block(Block::default().borders(Borders::LEFT).title(" keys "))
        .wrap(Wrap { trim: true });
    frame.render_widget(help, columns[1]);
}

fn render_compact(frame: &mut Frame<'_>, area: Rect, app: &App, palette: Theme, phone: bool) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(if app.warning.is_some() { 2 } else { 1 }),
        ])
        .split(area);
    frame.render_widget(Paragraph::new("codex-mux").style(palette.accent), chunks[0]);
    render_list(frame, chunks[1], app, palette);
    let footer = if matches!(app.mode, Mode::ConfirmClose(_)) {
        Paragraph::new("press x/Enter again to close · Esc cancels").style(palette.warning)
    } else if let Some(warning) = &app.warning {
        Paragraph::new(sanitized(warning))
            .style(palette.warning)
            .wrap(Wrap { trim: true })
    } else if phone {
        Paragraph::new("↕/jk move · Enter open · n new · r resume · R rename · x close · c config")
            .style(palette.muted)
    } else {
        Paragraph::new(
            "jk/↕ move  Enter switch  n new  r resume  R rename  x close  t theme  c config  q quit",
        )
        .style(palette.muted)
    };
    frame.render_widget(footer, chunks[2]);
}

fn render_tiny(frame: &mut Frame<'_>, area: Rect, app: &App, palette: Theme) {
    let title = app
        .selected_index()
        .and_then(|index| app.panes.get(index))
        .map(|pane| sanitized(&pane.display_title()))
        .unwrap_or_else(|| "No Codex panes".to_owned());
    let lines = if matches!(app.mode, Mode::ConfirmClose(_)) {
        vec![
            Line::styled("Close pane?", palette.warning),
            Line::from(title),
            Line::styled("x=yes Esc=no", palette.muted),
        ]
    } else {
        vec![
            Line::styled("codex-mux", palette.accent),
            Line::styled(title, palette.selected),
            Line::styled("↕ open n r R x t c q", palette.muted),
        ]
    };
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), area);
}

fn render_list(frame: &mut Frame<'_>, area: Rect, app: &App, palette: Theme) {
    const HIGHLIGHT_SYMBOL: &str = "› ";
    const PATH_INDENT: &str = "  ";

    // Ratatui reserves the highlight-symbol width for every item whenever a selection exists.
    // Budget inside that shared item area so selected and unselected rows elide identically.
    let highlight_width = UnicodeWidthStr::width(HIGHLIGHT_SYMBOL) as u16;
    let path_indent_width = UnicodeWidthStr::width(PATH_INDENT) as u16;
    let title_width = usize::from(area.width.saturating_sub(highlight_width));
    let path_width = usize::from(
        area.width
            .saturating_sub(highlight_width + path_indent_width),
    );
    let items = app.panes.iter().map(|pane| {
        let title = end_elide(&sanitized(&pane.display_title()), title_width);
        let path = start_elide(&sanitized(&pane.current_path.to_string_lossy()), path_width);
        ListItem::new(vec![
            Line::from(title),
            Line::styled(format!("{PATH_INDENT}{path}"), palette.muted),
        ])
    });
    let mut state = ListState::default().with_selected(app.selected_index());
    let list = List::new(items)
        .highlight_style(palette.selected)
        .highlight_symbol(HIGHLIGHT_SYMBOL)
        .block(Block::default().title(" sessions "));
    frame.render_stateful_widget(list, area, &mut state);
}

fn end_elide(value: &str, max_width: usize) -> String {
    elide(value, max_width, false)
}

fn start_elide(value: &str, max_width: usize) -> String {
    elide(value, max_width, true)
}

fn elide(value: &str, max_width: usize, from_start: bool) -> String {
    if UnicodeWidthStr::width(value) <= max_width {
        return value.to_owned();
    }
    if max_width == 0 {
        return String::new();
    }

    let available = max_width - UnicodeWidthStr::width("…");
    if from_start {
        let mut width = 0;
        let mut suffix = Vec::new();
        for grapheme in value.graphemes(true).rev() {
            let grapheme_width = UnicodeWidthStr::width(grapheme);
            if width + grapheme_width > available {
                break;
            }
            suffix.push(grapheme);
            width += grapheme_width;
        }
        format!("…{}", suffix.into_iter().rev().collect::<String>())
    } else {
        let mut width = 0;
        let mut prefix = String::new();
        for grapheme in value.graphemes(true) {
            let grapheme_width = UnicodeWidthStr::width(grapheme);
            if width + grapheme_width > available {
                break;
            }
            prefix.push_str(grapheme);
            width += grapheme_width;
        }
        prefix.push('…');
        prefix
    }
}

fn render_profile_picker(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    palette: Theme,
    selected: usize,
) {
    let height = (app.profiles.len() as u16 + 4).min(area.height).max(5);
    let popup = centered_rect(area, if area.width <= 62 { 96 } else { 72 }, height);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(" launch profile ")
        .borders(Borders::ALL)
        .border_style(palette.accent);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);
    let items = app.profiles.iter().enumerate().map(|(index, profile)| {
        let executable = profile
            .executable
            .as_ref()
            .map_or("configured codex".to_owned(), |path| {
                sanitized(&path.display().to_string())
            });
        let permissions = match profile.permissions {
            PermissionPreset::Standard => "standard",
            PermissionPreset::Yolo => "YOLO",
        };
        let style = if index == selected {
            palette.selected
        } else {
            palette.text
        };
        ListItem::new(Line::styled(
            format!(
                " {}  {:<16}  {} · {}",
                profile.key,
                sanitized(&profile.name),
                executable,
                permissions
            ),
            style,
        ))
    });
    let mut state = ListState::default().with_selected(Some(selected));
    frame.render_stateful_widget(List::new(items), chunks[0], &mut state);
    frame.render_widget(
        Paragraph::new("key launch · ↑/↓ · Enter · a add · e edit · Esc").style(palette.muted),
        chunks[1],
    );
}

fn render_profile_editor(
    frame: &mut Frame<'_>,
    area: Rect,
    editor: &ProfileEditor,
    palette: Theme,
) {
    let popup = centered_rect(
        area,
        if area.width <= 62 { 96 } else { 72 },
        11.min(area.height),
    );
    frame.render_widget(Clear, popup);
    let field = |label: &str, value: String, candidate: EditorField| {
        let style = if editor.field == candidate {
            palette.selected
        } else {
            palette.text
        };
        Line::styled(format!(" {label:<12} {value}"), style)
    };
    let mut lines = vec![
        field("Name", sanitized(&editor.name), EditorField::Name),
        field("Key", editor.key.clone(), EditorField::Key),
        field(
            "Binary",
            if editor.executable.is_empty() {
                "configured codex".to_owned()
            } else {
                sanitized(&editor.executable)
            },
            EditorField::Executable,
        ),
        field(
            "Permissions",
            match editor.permissions {
                PermissionPreset::Standard => "standard".to_owned(),
                PermissionPreset::Yolo => "YOLO".to_owned(),
            },
            EditorField::Permissions,
        ),
        Line::default(),
    ];
    if let Some(error) = &editor.error {
        lines.push(Line::styled(sanitized(error), palette.warning));
    } else {
        lines.push(Line::styled(
            "Tab/↑↓ field · ←/→ permissions · Enter save · Esc cancel",
            palette.muted,
        ));
    }
    let title = if editor.original.is_some() {
        " edit profile "
    } else {
        " add profile "
    };
    let form = Paragraph::new(lines).block(
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(palette.accent),
    );
    frame.render_widget(form, popup);
}

fn render_manual_rename(frame: &mut Frame<'_>, area: Rect, editor: &ManualRename, palette: Theme) {
    let popup = centered_rect(
        area,
        if area.width <= 62 { 96 } else { 56 },
        if editor.error.is_some() || editor.unpin_unavailable_reason.is_some() {
            8
        } else {
            7
        },
    );
    frame.render_widget(Clear, popup);
    let mut lines = vec![Line::styled(
        format!(" Name  {}", sanitized(&editor.title)),
        palette.selected,
    )];
    if let Some(error) = &editor.error {
        lines.push(Line::styled(sanitized(error), palette.warning));
    } else {
        let help = if editor.can_unpin {
            "c clear · empty Enter unpin · Esc cancel"
        } else {
            "c clear · Enter save · Esc cancel"
        };
        lines.push(Line::styled(help, palette.muted));
        if let Some(reason) = editor.unpin_unavailable_reason {
            lines.push(Line::styled(reason, palette.warning));
        }
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title(" rename session ")
                    .borders(Borders::ALL)
                    .border_style(palette.accent),
            )
            .wrap(Wrap { trim: true }),
        popup,
    );
}

fn validate_manual_title(title: &str) -> std::result::Result<String, &'static str> {
    let title = title.trim();
    if title.is_empty() {
        return Err("Name must not be empty");
    }
    if title.chars().count() > MAX_MANUAL_TITLE_CHARS {
        return Err("Name is too long");
    }
    if title
        .chars()
        .any(|character| character.is_control() || is_unsafe_format_character(character))
    {
        return Err("Name contains unsafe control characters");
    }
    Ok(title.to_owned())
}

fn is_unsafe_format_character(character: char) -> bool {
    matches!(
        character,
        '\u{2028}' | '\u{2029}'
            | '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{206f}' | '\u{feff}'
    )
}

fn render_theme_picker(frame: &mut Frame<'_>, area: Rect, app: &App, palette: Theme) {
    let popup = centered_rect(area, if area.width <= 62 { 92 } else { 48 }, 9);
    frame.render_widget(Clear, popup);
    let lines = ThemeId::ALL.into_iter().map(|candidate| {
        let marker = if candidate == app.theme { "› " } else { "  " };
        let style = if candidate == app.theme {
            palette.selected
        } else {
            Style::default()
        };
        Line::styled(format!("{marker}{candidate}"), style)
    });
    let picker = Paragraph::new(lines.collect::<Vec<_>>())
        .block(
            Block::default()
                .title(" theme · live preview ")
                .borders(Borders::ALL)
                .border_style(palette.accent),
        )
        .alignment(Alignment::Left);
    frame.render_widget(picker, popup);
}

fn render_configuration(frame: &mut Frame<'_>, area: Rect, app: &App, palette: Theme) {
    let constrained = area.width <= 62 || area.height < 16;
    let popup = centered_rect(
        area,
        if constrained { 96 } else { 66 },
        if constrained { 8 } else { 12 },
    );
    frame.render_widget(Clear, popup);
    let state = if app.smart_naming_pending {
        "STOPPING"
    } else if app.smart_naming {
        "ON"
    } else {
        "OFF (default)"
    };
    let lines = if area.width < 40 {
        vec![
            Line::styled(format!(" N Smart names: {state}"), palette.selected),
            Line::from("GPT-5.6 Luna gets chat"),
            Line::from("Uses allowance · not stored"),
            Line::from("Manual/error keeps title"),
            Line::styled("N toggle · Esc close", palette.muted),
        ]
    } else if constrained {
        vec![
            Line::styled(format!(" N  Smart names: {state}"), palette.selected),
            Line::from("Shares bounded completed chat with GPT-5.6 Luna."),
            Line::from("Uses Codex allowance; codex-mux stores no chat."),
            Line::from("No restart. Errors/manual names keep current title."),
            Line::styled("N toggle · C/Esc close", palette.muted),
        ]
    } else {
        vec![
            Line::styled(
                format!(" N  Conversation-aware names  {state}"),
                palette.selected,
            ),
            Line::default(),
            Line::from("Reads completed Codex conversation text and sends a bounded excerpt"),
            Line::from("to GPT-5.6 Luna using your existing Codex login. Content is not saved"),
            Line::from("by codex-mux. Model usage may count against your Codex allowance."),
            Line::default(),
            Line::from("Works for running sessions without restart. Failures keep current names."),
            Line::from("Manually renamed windows are never overwritten."),
            Line::default(),
            Line::styled("N toggle · C/Esc close", palette.muted),
        ]
    };
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: true }).block(
            Block::default()
                .title(" configuration ")
                .borders(Borders::ALL)
                .border_style(palette.accent),
        ),
        popup,
    );
}

fn centered_rect(area: Rect, width_percent: u16, height: u16) -> Rect {
    let width = area.width.saturating_mul(width_percent).saturating_div(100);
    let width = width.max(1).min(area.width);
    let height = height.min(area.height).max(1);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

fn sanitized(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                '�'
            } else {
                character
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        sync::{Arc, Mutex},
    };

    use crossterm::event::{KeyEventKind, KeyModifiers};

    use super::terminal::{TerminalControl, with_restoration};
    use super::{
        Action, App, ColorPolicy, KeyCode, KeyEvent, LayoutKind, Pane, PaneId, TerminalSize,
        ThemeId, end_elide, layout_kind, start_elide,
    };
    use crate::domain::SessionId;
    use unicode_width::UnicodeWidthStr;

    fn pane(id: &str, title: &str) -> Pane {
        Pane {
            id: PaneId::new(id).unwrap(),
            session_id: SessionId::new("$1").unwrap(),
            title: Some(title.to_owned()),
            generated_title: None,
            generated_at_unix: None,
            immediate_naming: false,
            manual_name: false,

            manual_name_source: None,

            manual_name_pid: None,

            manual_name_session: None,

            pane_pid: 100,
            current_path: PathBuf::from(format!("/work/{title}")),
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn elision_preserves_direction_graphemes_and_terminal_width() {
        assert_eq!(end_elide("short", 5), "short");
        assert_eq!(end_elide("abcdef", 5), "abcd…");
        assert_eq!(start_elide("/alpha/beta", 6), "…/beta");
        assert_eq!(end_elide("e\u{301}clair", 4), "e\u{301}cl…");
        assert_eq!(end_elide("👩‍💻abc", 4), "👩‍💻a…");

        for (value, width) in [
            (end_elide("改善 session title", 8), 8),
            (start_elide("/work/改善/session", 8), 8),
            (end_elide("👩‍💻 builds", 6), 6),
        ] {
            assert!(UnicodeWidthStr::width(value.as_str()) <= width);
        }
    }

    #[test]
    fn elision_handles_empty_and_tiny_budgets() {
        assert_eq!(end_elide("", 0), "");
        assert_eq!(end_elide("abc", 0), "");
        assert_eq!(start_elide("abc", 0), "");
        assert_eq!(end_elide("abc", 1), "…");
        assert_eq!(start_elide("abc", 1), "…");
    }

    #[test]
    fn responsive_breakpoints_include_phone_and_tiny() {
        assert_eq!(
            layout_kind(TerminalSize {
                width: 120,
                height: 40
            }),
            LayoutKind::Wide
        );
        assert_eq!(
            layout_kind(TerminalSize {
                width: 89,
                height: 40
            }),
            LayoutKind::Compact
        );
        assert_eq!(
            layout_kind(TerminalSize {
                width: 62,
                height: 28
            }),
            LayoutKind::Phone
        );
        assert_eq!(
            layout_kind(TerminalSize {
                width: 39,
                height: 28
            }),
            LayoutKind::Tiny
        );
    }

    #[test]
    fn selection_survives_reordering_and_uses_nearest_row_on_disappearance() {
        let mut app = App::new(
            vec![pane("%1", "one"), pane("%2", "two"), pane("%3", "three")],
            ThemeId::default(),
            None,
        );
        app.handle_key(key(KeyCode::Char('j')));
        app.replace_panes(vec![
            pane("%3", "three"),
            pane("%2", "two"),
            pane("%1", "one"),
        ]);
        assert_eq!(app.selected_pane_id().unwrap().as_str(), "%2");
        app.replace_panes(vec![pane("%3", "three"), pane("%1", "one")]);
        assert_eq!(app.selected_pane_id().unwrap().as_str(), "%1");
    }

    #[test]
    fn explicit_selection_uses_requested_pane_and_ignores_missing_panes() {
        let mut app = App::new(
            vec![pane("%1", "one"), pane("%2", "two")],
            ThemeId::default(),
            None,
        );

        app.select_pane(&PaneId::new("%2").unwrap());
        assert_eq!(app.selected_pane_id().unwrap().as_str(), "%2");

        app.select_pane(&PaneId::new("%3").unwrap());
        assert_eq!(app.selected_pane_id().unwrap().as_str(), "%2");
    }

    #[test]
    fn close_requires_a_second_deliberate_key() {
        let mut app = App::new(vec![pane("%1", "one")], ThemeId::default(), None);
        assert_eq!(app.handle_key(key(KeyCode::Char('x'))), None);
        assert_eq!(
            app.handle_key(KeyEvent::new_with_kind(
                KeyCode::Char('x'),
                KeyModifiers::NONE,
                KeyEventKind::Repeat,
            )),
            None
        );
        assert_eq!(
            app.handle_key(key(KeyCode::Char('x'))),
            Some(Action::Close(PaneId::new("%1").unwrap()))
        );
    }

    #[test]
    fn forced_monochrome_disables_colored_preview_and_persistence() {
        let mut app = App::with_color_policy(
            vec![],
            ThemeId::EmberOrange,
            None,
            ColorPolicy::ForceMonochrome,
        );
        assert_eq!(app.active_theme(), ThemeId::Monochrome);
        assert_eq!(app.handle_key(key(KeyCode::Char('t'))), None);
        assert_eq!(app.handle_key(key(KeyCode::Char('j'))), None);
        assert_eq!(app.active_theme(), ThemeId::Monochrome);
        assert_eq!(app.handle_key(key(KeyCode::Enter)), None);
    }

    #[test]
    fn theme_picker_previews_then_reverts_or_persists() {
        let mut app = App::new(vec![], ThemeId::AdaptiveCyan, None);
        app.handle_key(key(KeyCode::Char('t')));
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.active_theme(), ThemeId::BlueCommandPalette);
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.active_theme(), ThemeId::AdaptiveCyan);
        app.handle_key(key(KeyCode::Char('t')));
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Some(Action::PersistTheme(ThemeId::BlueCommandPalette))
        );
    }

    #[derive(Clone)]
    struct FakeTerminal(Arc<Mutex<Vec<&'static str>>>);

    impl TerminalControl for FakeTerminal {
        fn enter(&mut self) -> std::io::Result<()> {
            self.0.lock().unwrap().push("enter");
            Ok(())
        }
        fn leave(&mut self) -> std::io::Result<()> {
            self.0.lock().unwrap().push("leave");
            Ok(())
        }
    }

    #[test]
    fn terminal_restores_after_success_error_and_panic() {
        for outcome in ["ok", "error"] {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let result = with_restoration(FakeTerminal(calls.clone()), |_| {
                if outcome == "ok" {
                    Ok(())
                } else {
                    Err("failed")
                }
            })
            .unwrap();
            assert_eq!(result.is_ok(), outcome == "ok");
            assert_eq!(*calls.lock().unwrap(), vec!["enter", "leave"]);
        }
        let calls = Arc::new(Mutex::new(Vec::new()));
        let _ = std::panic::catch_unwind({
            let calls = calls.clone();
            move || {
                let _ = with_restoration(FakeTerminal(calls), |_| -> Result<(), ()> {
                    panic!("boom")
                });
            }
        });
        assert_eq!(*calls.lock().unwrap(), vec!["enter", "leave"]);
    }
}
