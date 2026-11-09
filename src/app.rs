use crate::model::{AddIssueDraft, Dependency, EditField, Issue, IssueGraph, DEFAULT_STATUSES};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

const EDIT_SEQUENCE_TIMEOUT: Duration = Duration::from_millis(500);

pub const ISSUE_TYPES: [&str; 6] = ["task", "bug", "feature", "epic", "chore", "decision"];
pub const PRIORITIES: [i32; 5] = [0, 1, 2, 3, 4];
/// P1, the priority preselected when an issue has none of `PRIORITIES`.
pub const DEFAULT_PRIORITY_INDEX: usize = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeRow {
    pub issue_id: String,
    pub path: Vec<String>,
    pub prefix: String,
    pub cycle: bool,
}

impl TreeRow {
    /// The synthetic first row of the tree; Enter on it creates a top-level issue.
    fn create_entry() -> Self {
        Self {
            issue_id: String::new(),
            path: Vec::new(),
            prefix: String::new(),
            cycle: false,
        }
    }

    pub fn is_create_entry(&self) -> bool {
        self.path.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DetailFrame {
    pub issue_id: String,
    pub dependency_cursor: usize,
    pub scroll: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    Tree,
    Detail,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddIssueStep {
    Title,
    Description,
    IssueType,
    Priority,
}

impl AddIssueStep {
    pub fn number(self) -> usize {
        match self {
            Self::Title => 1,
            Self::Description => 2,
            Self::IssueType => 3,
            Self::Priority => 4,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddIssueField {
    Title,
    Description,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AddIssueFlow {
    pub parent_id: Option<String>,
    pub title: String,
    pub description: String,
    pub step: AddIssueStep,
    pub issue_type_index: usize,
    pub priority_index: usize,
    confirming_cancel: bool,
}

impl AddIssueFlow {
    pub fn issue_type(&self) -> &'static str {
        ISSUE_TYPES[self.issue_type_index]
    }

    pub fn priority(&self) -> i32 {
        PRIORITIES[self.priority_index]
    }

    pub fn is_confirming_cancel(&self) -> bool {
        self.confirming_cancel
    }

    fn draft(&self) -> AddIssueDraft {
        AddIssueDraft {
            parent_id: self.parent_id.clone(),
            title: self.title.trim().to_string(),
            description: self.description.trim_end().to_string(),
            issue_type: self.issue_type().to_string(),
            priority: self.priority(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChoiceKind {
    Status,
    Priority,
}

/// A single choice that updates one field of one issue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    pub kind: ChoiceKind,
    pub issue_id: String,
    pub options: Vec<String>,
    pub index: usize,
}

/// A screen that replaces the tree or task view until it is applied or
/// cancelled; either way bew returns to where it was.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    Choose(Choice),
    Close { issue_id: String },
    AddIssue(AddIssueFlow),
}

impl Step {
    /// The existing issue the step acts on; an add-issue step has none.
    fn issue_id(&self) -> Option<&str> {
        match self {
            Self::Choose(choice) => Some(&choice.issue_id),
            Self::Close { issue_id } => Some(issue_id),
            Self::AddIssue(_) => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Quit,
    CloseIssue(String),
    Edit(EditField),
    EditAddIssue(AddIssueField),
    CreateIssue(AddIssueDraft),
    SetStatus(String, String),
    SetPriority(String, i32),
    /// Open `command` in `$EDITOR`; run what is saved in a new tmux pane
    /// bound to the issue.
    StartAgent {
        issue_id: String,
        command: String,
    },
    /// Show the tmux pane with this ID.
    FocusAgent(String),
}

/// The mutually exclusive input modes over the tree and task view.
enum Mode {
    Normal,
    Search {
        query: String,
        origin_cursor: usize,
        origin_scroll: usize,
    },
    Step(Step),
}

pub struct App {
    pub graph: IssueGraph,
    pub rows: Vec<TreeRow>,
    pub cursor: usize,
    pub scroll: usize,
    pub viewport: usize,
    mode: Mode,
    tree_rows: Vec<TreeRow>,
    expanded: HashSet<Vec<String>>,
    history: Vec<DetailFrame>,
    edit_key_started: Option<Instant>,
    status_message: Option<String>,
    /// What bew is waiting for; shows when there is no status message, and
    /// keys do not clear it.
    busy: Option<String>,
    statuses: Vec<String>,
    /// Bead ID → tmux pane ID, for the beads that have a live agent pane.
    panes: HashMap<String, String>,
    tmux: bool,
}

impl App {
    pub fn new(graph: IssueGraph) -> Self {
        let mut app = Self {
            graph,
            rows: Vec::new(),
            cursor: 0,
            scroll: 0,
            viewport: 1,
            mode: Mode::Normal,
            tree_rows: Vec::new(),
            expanded: HashSet::new(),
            history: Vec::new(),
            edit_key_started: None,
            status_message: None,
            busy: None,
            statuses: DEFAULT_STATUSES.map(str::to_string).to_vec(),
            panes: HashMap::new(),
            tmux: false,
        };
        app.rebuild_rows();
        // Start on the first issue rather than the synthetic create entry.
        app.cursor = 1.min(app.rows.len() - 1);
        app
    }

    pub fn screen(&self) -> Screen {
        if self.history.is_empty() {
            Screen::Tree
        } else {
            Screen::Detail
        }
    }

    /// The step that replaces the screen, if one is open.
    pub fn step(&self) -> Option<&Step> {
        match &self.mode {
            Mode::Step(step) => Some(step),
            _ => None,
        }
    }

    pub fn current_tree_issue(&self) -> Option<&Issue> {
        self.rows
            .get(self.cursor)
            .and_then(|row| self.graph.issue(&row.issue_id))
    }

    pub fn search_query(&self) -> Option<&str> {
        match &self.mode {
            Mode::Search { query, .. } => Some(query),
            _ => None,
        }
    }

    pub fn current_detail_issue(&self) -> Option<&Issue> {
        self.history
            .last()
            .and_then(|frame| self.graph.issue(&frame.issue_id))
    }

    /// The issue the edit/status/priority/close/agent keys act on: the
    /// selected tree row or the issue in the task view, depending on the
    /// visible screen.
    pub fn current_issue(&self) -> Option<&Issue> {
        match self.screen() {
            Screen::Tree => self.current_tree_issue(),
            Screen::Detail => self.current_detail_issue(),
        }
    }

    pub fn detail_frame(&self) -> Option<&DetailFrame> {
        self.history.last()
    }

    pub fn detail_frame_mut(&mut self) -> Option<&mut DetailFrame> {
        self.history.last_mut()
    }

    pub fn is_confirming_close(&self) -> bool {
        self.closing_issue_id().is_some()
    }

    pub fn closing_issue_id(&self) -> Option<&str> {
        match self.step() {
            Some(Step::Close { issue_id }) => Some(issue_id),
            _ => None,
        }
    }

    pub fn add_issue_flow(&self) -> Option<&AddIssueFlow> {
        match self.step() {
            Some(Step::AddIssue(flow)) => Some(flow),
            _ => None,
        }
    }

    pub fn set_add_issue_field(&mut self, field: AddIssueField, value: String) {
        let Mode::Step(Step::AddIssue(flow)) = &mut self.mode else {
            return;
        };
        match field {
            AddIssueField::Title => {
                flow.title = value
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ");
            }
            AddIssueField::Description => flow.description = value.trim_end().to_string(),
        }
    }

    pub fn add_issue_field(&self, field: AddIssueField) -> Option<&str> {
        self.add_issue_flow().map(|flow| match field {
            AddIssueField::Title => flow.title.as_str(),
            AddIssueField::Description => flow.description.as_str(),
        })
    }

    pub fn finish_add_issue(&mut self) {
        if matches!(self.mode, Mode::Step(Step::AddIssue(_))) {
            self.mode = Mode::Normal;
        }
    }

    pub fn status_message(&self) -> Option<&str> {
        self.status_message.as_deref().or(self.busy.as_deref())
    }

    pub fn set_busy(&mut self, busy: Option<String>) {
        self.busy = busy;
    }

    pub fn set_status(&mut self, message: String) {
        self.status_message = Some(message);
    }

    pub fn clear_status(&mut self) {
        self.status_message = None;
    }

    /// Replaces the statuses offered by `s`, once `bd statuses` answers.
    pub fn set_statuses(&mut self, statuses: Vec<String>) {
        if !statuses.is_empty() {
            self.statuses = statuses;
        }
    }

    /// Whether bew runs inside tmux, so agent panes can be created.
    pub fn tmux(&self) -> bool {
        self.tmux
    }

    pub fn set_tmux(&mut self, tmux: bool) {
        self.tmux = tmux;
    }

    /// Replaces the bead → pane map; returns whether it changed.
    pub fn set_panes(&mut self, panes: HashMap<String, String>) -> bool {
        if panes == self.panes {
            return false;
        }
        self.panes = panes;
        true
    }

    /// The live agent pane bound to `issue_id`, if there is one.
    pub fn pane(&self, issue_id: &str) -> Option<&str> {
        self.panes.get(issue_id).map(String::as_str)
    }

    pub fn can_close_current_issue(&self) -> bool {
        self.current_detail_issue()
            .is_some_and(|issue| self.graph.is_listed(&issue.id))
    }

    pub fn row_is_expanded(&self, row: &TreeRow) -> bool {
        self.expanded.contains(&row.path)
    }

    pub fn row_has_children(&self, row: &TreeRow) -> bool {
        !row.cycle && !self.graph.tree_children(&row.issue_id).is_empty()
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        self.status_message = None;
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.mode = Mode::Normal;
            self.edit_key_started = None;
            return Action::Quit;
        }
        match self.mode {
            Mode::Step(_) => self.handle_step_key(key),
            Mode::Search { .. } => self.handle_search_key(key),
            Mode::Normal => {
                if let Some(action) = self.resolve_edit_chord(&key) {
                    return action;
                }
                if let Some(action) = self.handle_issue_key(key.code) {
                    return action;
                }
                match self.screen() {
                    Screen::Tree => self.handle_tree_key(key),
                    Screen::Detail => self.handle_detail_key(key),
                }
            }
        }
    }

    /// Adds pasted text to the text field being edited, if there is one.
    /// Single-line fields get the lines joined with spaces.
    pub fn handle_paste(&mut self, text: &str) {
        self.status_message = None;
        let line = text.replace("\r\n", " ").replace(['\r', '\n'], " ");
        match &mut self.mode {
            Mode::Search { query, .. } => {
                query.push_str(line.trim());
                self.rebuild_search_rows();
            }
            Mode::Step(Step::AddIssue(flow)) if !flow.confirming_cancel => match flow.step {
                AddIssueStep::Title => flow.title.push_str(&line),
                AddIssueStep::Description => flow.description.push_str(text),
                AddIssueStep::IssueType | AddIssueStep::Priority => {}
            },
            _ => {}
        }
    }

    /// Keys that act on `current_issue()` the same way on both screens;
    /// `None` means `code` was not one of them.
    fn handle_issue_key(&mut self, code: KeyCode) -> Option<Action> {
        match code {
            KeyCode::Char('e') if self.current_issue().is_some() => {
                self.edit_key_started = Some(Instant::now());
            }
            KeyCode::Char('s') => self.start_choice(ChoiceKind::Status),
            KeyCode::Char('p') => self.start_choice(ChoiceKind::Priority),
            KeyCode::Char('x') => self.start_close_confirmation(),
            KeyCode::Char('a') => return Some(self.start_agent()),
            KeyCode::Char('f') => return Some(self.focus_agent()),
            _ => return None,
        }
        Some(Action::None)
    }

    pub fn pending_key_timeout(&self) -> Option<Duration> {
        self.edit_key_started
            .map(|started| EDIT_SEQUENCE_TIMEOUT.saturating_sub(started.elapsed()))
    }

    pub fn flush_pending_key(&mut self) -> Action {
        if self.edit_key_started.take().is_some() {
            Action::Edit(EditField::Description)
        } else {
            Action::None
        }
    }

    /// Resolves a pending `e` chord: `et` edits the title, `ee` (or the
    /// timeout) edits the description, anything else cancels the chord and is
    /// handled normally by the caller.
    fn resolve_edit_chord(&mut self, key: &KeyEvent) -> Option<Action> {
        let started = self.edit_key_started.take()?;
        if started.elapsed() >= EDIT_SEQUENCE_TIMEOUT {
            return Some(Action::Edit(EditField::Description));
        }
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        if plain && key.code == KeyCode::Char('t') {
            return Some(Action::Edit(EditField::Title));
        }
        if plain && key.code == KeyCode::Char('e') {
            return Some(Action::Edit(EditField::Description));
        }
        None
    }

    fn handle_tree_key(&mut self, key: KeyEvent) -> Action {
        if key.code == KeyCode::Char('/') {
            self.start_search();
            return Action::None;
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('d') if ctrl => self.move_cursor(self.viewport as isize / 2),
            KeyCode::Char('u') if ctrl => self.move_cursor(-(self.viewport as isize) / 2),
            KeyCode::Char('j') | KeyCode::Down => self.move_cursor(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_cursor(-1),
            KeyCode::PageDown => self.move_cursor(self.viewport as isize),
            KeyCode::PageUp => self.move_cursor(-(self.viewport as isize)),
            KeyCode::Char('g') | KeyCode::Home => self.cursor = 0,
            KeyCode::Char('G') | KeyCode::End => self.cursor = self.rows.len() - 1,
            KeyCode::Tab => self.toggle(),
            KeyCode::Char('l') | KeyCode::Right => self.expand_or_enter(),
            KeyCode::Char('h') | KeyCode::Left => self.collapse_or_parent(),
            KeyCode::Enter => self.open_selected_row(),
            KeyCode::Char('+') => {
                let parent_id = self.current_tree_issue().map(|issue| issue.id.clone());
                self.start_add_issue(parent_id);
            }
            KeyCode::Char('q') | KeyCode::Esc => return Action::Quit,
            _ => {}
        }
        Action::None
    }

    fn handle_search_key(&mut self, key: KeyEvent) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.cancel_search(),
            KeyCode::Enter => self.open_search_result(),
            KeyCode::Char('d') if ctrl => self.move_cursor(self.viewport as isize / 2),
            KeyCode::Char('u') if ctrl => {
                if let Mode::Search { query, .. } = &mut self.mode {
                    query.clear();
                }
                self.rebuild_search_rows();
            }
            KeyCode::Down => self.move_cursor(1),
            KeyCode::Up => self.move_cursor(-1),
            KeyCode::PageDown => self.move_cursor(self.viewport as isize),
            KeyCode::PageUp => self.move_cursor(-(self.viewport as isize)),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.rows.len().saturating_sub(1),
            KeyCode::Backspace => {
                if let Mode::Search { query, .. } = &mut self.mode {
                    query.pop();
                }
                self.rebuild_search_rows();
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if let Mode::Search { query, .. } = &mut self.mode {
                    query.push(character);
                }
                self.rebuild_search_rows();
            }
            _ => {}
        }
        Action::None
    }

    fn handle_detail_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.move_dependency_cursor(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_dependency_cursor(-1),
            KeyCode::Char('g') | KeyCode::Home => {
                if let Some(frame) = self.history.last_mut() {
                    frame.dependency_cursor = 0;
                }
            }
            KeyCode::Char('G') | KeyCode::End => {
                let last = self
                    .current_detail_issue()
                    .map(|issue| issue.dependencies.len().saturating_sub(1))
                    .unwrap_or(0);
                if let Some(frame) = self.history.last_mut() {
                    frame.dependency_cursor = last;
                }
            }
            KeyCode::Enter => self.open_selected_dependency(),
            KeyCode::Char('+') => {
                if let Some(parent_id) = self.current_detail_issue().map(|issue| issue.id.clone()) {
                    self.start_add_issue(Some(parent_id));
                }
            }
            KeyCode::Backspace => {
                self.history.pop();
            }
            KeyCode::Esc => self.history.clear(),
            KeyCode::Char('q') => return Action::Quit,
            _ => {}
        }
        Action::None
    }

    fn handle_step_key(&mut self, key: KeyEvent) -> Action {
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        let Mode::Step(step) = &mut self.mode else {
            return Action::None;
        };
        let action = match step {
            Step::Choose(choice) => match key.code {
                KeyCode::Esc => Some(Action::None),
                KeyCode::Enter => Some(self.apply_choice()),
                code => {
                    move_selection(&mut choice.index, choice.options.len(), code);
                    None
                }
            },
            Step::Close { issue_id } => match key.code {
                KeyCode::Char('y' | 'Y') => Some(Action::CloseIssue(issue_id.clone())),
                KeyCode::Char('n' | 'N') | KeyCode::Esc => Some(Action::None),
                _ => None,
            },
            Step::AddIssue(flow) => {
                let (action, done) = handle_add_issue_key(flow, key, plain);
                if done {
                    self.mode = Mode::Normal;
                }
                return action;
            }
        };
        // `Some` means the step is done, applied or cancelled.
        match action {
            Some(action) => {
                self.mode = Mode::Normal;
                action
            }
            None => Action::None,
        }
    }

    /// The update for the selected option, or nothing when it is the value
    /// the issue already has.
    fn apply_choice(&self) -> Action {
        let Mode::Step(Step::Choose(choice)) = &self.mode else {
            return Action::None;
        };
        let Some(option) = choice.options.get(choice.index) else {
            return Action::None;
        };
        let unchanged = self
            .graph
            .issue(&choice.issue_id)
            .and_then(|issue| current_choice_index(choice.kind, issue, &choice.options))
            == Some(choice.index);
        if unchanged {
            return Action::None;
        }
        match choice.kind {
            ChoiceKind::Status => Action::SetStatus(choice.issue_id.clone(), option.clone()),
            ChoiceKind::Priority => {
                Action::SetPriority(choice.issue_id.clone(), PRIORITIES[choice.index])
            }
        }
    }

    fn start_choice(&mut self, kind: ChoiceKind) {
        let Some(issue) = self.current_issue() else {
            return;
        };
        let options: Vec<String> = match kind {
            ChoiceKind::Status => self.statuses.clone(),
            ChoiceKind::Priority => PRIORITIES.iter().map(|p| format!("P{p}")).collect(),
        };
        let index = current_choice_index(kind, issue, &options).unwrap_or(match kind {
            ChoiceKind::Status => 0,
            ChoiceKind::Priority => DEFAULT_PRIORITY_INDEX,
        });
        let issue_id = issue.id.clone();
        self.mode = Mode::Step(Step::Choose(Choice {
            kind,
            issue_id,
            options,
            index,
        }));
    }

    fn start_agent(&mut self) -> Action {
        let Some(issue_id) = self.current_issue().map(|issue| issue.id.clone()) else {
            return Action::None;
        };
        if !self.tmux {
            self.set_status("agent panes need tmux: run bew inside tmux".to_string());
            return Action::None;
        }
        let command = agent_command(&issue_id);
        Action::StartAgent { issue_id, command }
    }

    fn focus_agent(&mut self) -> Action {
        let Some(issue_id) = self.current_issue().map(|issue| issue.id.clone()) else {
            return Action::None;
        };
        if !self.tmux {
            self.set_status("agent panes need tmux: run bew inside tmux".to_string());
            return Action::None;
        }
        match self.pane(&issue_id) {
            Some(pane_id) => Action::FocusAgent(pane_id.to_string()),
            None => {
                self.set_status(format!("no agent pane for {issue_id}"));
                Action::None
            }
        }
    }

    fn move_cursor(&mut self, delta: isize) {
        let last = self.rows.len().saturating_sub(1) as isize;
        self.cursor = (self.cursor as isize + delta).clamp(0, last) as usize;
    }

    fn move_dependency_cursor(&mut self, delta: isize) {
        let last = self
            .current_detail_issue()
            .map(|issue| issue.dependencies.len().saturating_sub(1))
            .unwrap_or(0) as isize;
        if let Some(frame) = self.history.last_mut() {
            frame.dependency_cursor =
                (frame.dependency_cursor as isize + delta).clamp(0, last) as usize;
        }
    }

    /// Opens the selected issue, or starts a top-level add on the create entry.
    fn open_selected_row(&mut self) {
        let Some(row) = self.rows.get(self.cursor) else {
            return;
        };
        if row.is_create_entry() {
            self.start_add_issue(None);
            return;
        }
        self.history.push(DetailFrame {
            issue_id: row.issue_id.clone(),
            dependency_cursor: 0,
            scroll: 0,
        });
    }

    fn start_close_confirmation(&mut self) {
        let issue_id = self
            .current_issue()
            .filter(|issue| self.graph.is_listed(&issue.id))
            .map(|issue| issue.id.clone());
        if let Some(issue_id) = issue_id {
            self.mode = Mode::Step(Step::Close { issue_id });
        }
    }

    fn start_add_issue(&mut self, parent_id: Option<String>) {
        self.mode = Mode::Step(Step::AddIssue(AddIssueFlow {
            parent_id,
            title: String::new(),
            description: String::new(),
            step: AddIssueStep::Title,
            issue_type_index: 0,
            priority_index: DEFAULT_PRIORITY_INDEX,
            confirming_cancel: false,
        }));
    }

    pub fn return_to_tree(&mut self) {
        self.history.clear();
        self.mode = Mode::Normal;
        self.edit_key_started = None;
    }

    /// Replace data loaded from `bd` without resetting the user's place: the
    /// tree selection, an open search and an open step all survive. Exact
    /// paths win; issue IDs are the fallback when dependency changes move a
    /// branch.
    pub fn refresh_graph(&mut self, graph: IssueGraph) {
        // During a search the tree position is the one the search started from.
        let (tree_cursor, tree_scroll) = match &self.mode {
            Mode::Search {
                origin_cursor,
                origin_scroll,
                ..
            } => (*origin_cursor, *origin_scroll),
            _ => (self.cursor, self.scroll),
        };
        let tree_cursor = tree_cursor.min(self.tree_rows.len().saturating_sub(1));
        let selected = self.tree_rows.get(tree_cursor);
        let selected_path = selected.map(|row| row.path.clone());
        let selected_issue_id = selected.map(|row| row.issue_id.clone());
        let search_selection = match &self.mode {
            Mode::Search { .. } => self.rows.get(self.cursor).map(|row| row.issue_id.clone()),
            _ => None,
        };
        let viewport_offset = tree_cursor.saturating_sub(tree_scroll);
        let moved_expanded_issue_ids: HashSet<String> = self
            .expanded
            .iter()
            .filter(|path| !Self::tree_path_exists(&graph, path))
            .filter_map(|path| path.last().cloned())
            .collect();

        self.graph = graph;

        // Paths that left the tree must not linger: a later refresh could
        // recreate one and surprise-expand a branch the user never opened.
        let current_graph = &self.graph;
        self.expanded
            .retain(|path| Self::tree_path_exists(current_graph, path));

        let (rows, restored_expansions) = self.build_rows(Some(&moved_expanded_issue_ids));
        self.expanded.extend(restored_expansions);
        self.tree_rows = rows;

        let exact = selected_path
            .as_ref()
            .and_then(|path| self.tree_rows.iter().position(|row| &row.path == path));
        let by_issue = selected_issue_id.as_ref().and_then(|issue_id| {
            self.tree_rows
                .iter()
                .enumerate()
                .filter(|(_, row)| &row.issue_id == issue_id)
                .min_by_key(|(position, _)| position.abs_diff(tree_cursor))
                .map(|(position, _)| position)
        });
        let new_cursor = exact
            .or(by_issue)
            .unwrap_or(tree_cursor)
            .min(self.tree_rows.len().saturating_sub(1));
        let new_scroll = new_cursor.saturating_sub(viewport_offset);

        if let Mode::Search {
            origin_cursor,
            origin_scroll,
            ..
        } = &mut self.mode
        {
            *origin_cursor = new_cursor;
            *origin_scroll = new_scroll;
            self.filter_search_rows(search_selection.as_deref());
        } else {
            self.rows.clone_from(&self.tree_rows);
            self.cursor = new_cursor;
            self.scroll = new_scroll;
        }

        self.end_step_if_its_issue_left();

        self.history
            .retain(|frame| self.graph.issue(&frame.issue_id).is_some());
        for frame in &mut self.history {
            let last_dependency = self
                .graph
                .issue(&frame.issue_id)
                .map(|issue| issue.dependencies.len().saturating_sub(1))
                .unwrap_or(0);
            frame.dependency_cursor = frame.dependency_cursor.min(last_dependency);
        }
    }

    /// Ends a step whose issue was deleted, or closed while closing it.
    fn end_step_if_its_issue_left(&mut self) {
        let Some(step) = self.step() else {
            return;
        };
        let message = match step {
            Step::Close { issue_id } if !self.graph.is_listed(issue_id) => {
                format!("{issue_id} is no longer open")
            }
            step => match step.issue_id() {
                Some(issue_id) if self.graph.issue(issue_id).is_none() => {
                    format!("{issue_id} no longer exists")
                }
                _ => return,
            },
        };
        self.mode = Mode::Normal;
        self.set_status(message);
    }

    fn tree_path_exists(graph: &IssueGraph, path: &[String]) -> bool {
        let Some(root) = path.first() else {
            return false;
        };
        graph.roots().contains(root)
            && path.windows(2).all(|edge| {
                graph
                    .tree_children(&edge[0])
                    .iter()
                    .any(|child| child == &edge[1])
            })
    }

    fn start_search(&mut self) {
        self.mode = Mode::Search {
            query: String::new(),
            origin_cursor: self.cursor,
            origin_scroll: self.scroll,
        };
        self.rebuild_search_rows();
    }

    fn cancel_search(&mut self) {
        let Mode::Search {
            origin_cursor,
            origin_scroll,
            ..
        } = std::mem::replace(&mut self.mode, Mode::Normal)
        else {
            return;
        };
        self.rows.clone_from(&self.tree_rows);
        self.cursor = origin_cursor.min(self.rows.len().saturating_sub(1));
        self.scroll = origin_scroll;
    }

    fn open_search_result(&mut self) {
        let issue_id = self.rows.get(self.cursor).map(|row| row.issue_id.clone());
        self.cancel_search();
        if let Some(issue_id) = issue_id {
            self.history.push(DetailFrame {
                issue_id,
                dependency_cursor: 0,
                scroll: 0,
            });
        }
    }

    /// Filters again after the query changed, selecting the first match.
    fn rebuild_search_rows(&mut self) {
        self.filter_search_rows(None);
        self.scroll = 0;
    }

    /// Filters the tree rows by the query, keeping `keep` selected if it
    /// still matches.
    fn filter_search_rows(&mut self, keep: Option<&str>) {
        let Mode::Search { query, .. } = &self.mode else {
            return;
        };
        self.rows = self
            .tree_rows
            .iter()
            .filter(|row| !row.is_create_entry() && fuzzy_match(&row.issue_id, query))
            .cloned()
            .collect();
        self.cursor = keep
            .and_then(|issue_id| self.rows.iter().position(|row| row.issue_id == issue_id))
            .unwrap_or(0);
    }

    fn open_selected_dependency(&mut self) {
        let dependency = self.selected_dependency().cloned();
        if let Some(dependency) = dependency {
            self.history.push(DetailFrame {
                issue_id: dependency.id,
                dependency_cursor: 0,
                scroll: 0,
            });
        }
    }

    pub fn selected_dependency(&self) -> Option<&Dependency> {
        let frame = self.history.last()?;
        self.graph
            .issue(&frame.issue_id)?
            .dependencies
            .get(frame.dependency_cursor)
    }

    fn toggle(&mut self) {
        let Some(row) = self.rows.get(self.cursor).cloned() else {
            return;
        };
        if !self.row_has_children(&row) {
            return;
        }
        if !self.expanded.remove(&row.path) {
            self.expanded.insert(row.path);
        }
        self.rebuild_rows();
    }

    fn expand_or_enter(&mut self) {
        let Some(row) = self.rows.get(self.cursor).cloned() else {
            return;
        };
        if !self.row_has_children(&row) {
            return;
        }
        if self.expanded.insert(row.path.clone()) {
            self.rebuild_rows();
        } else if self.cursor + 1 < self.rows.len()
            && self.rows[self.cursor + 1].path.starts_with(&row.path)
        {
            self.cursor += 1;
        }
    }

    fn collapse_or_parent(&mut self) {
        let Some(row) = self.rows.get(self.cursor).cloned() else {
            return;
        };
        if self.expanded.remove(&row.path) {
            self.rebuild_rows();
            return;
        }
        if row.path.len() <= 1 {
            return;
        }
        let parent = &row.path[..row.path.len() - 1];
        if let Some(position) = self
            .rows
            .iter()
            .position(|candidate| candidate.path == parent)
        {
            self.cursor = position;
        }
    }

    fn build_rows(
        &self,
        expanded_issue_ids: Option<&HashSet<String>>,
    ) -> (Vec<TreeRow>, HashSet<Vec<String>>) {
        fn walk(
            graph: &IssueGraph,
            expanded: &HashSet<Vec<String>>,
            expanded_issue_ids: Option<&HashSet<String>>,
            path: &[String],
            prefix: &str,
            rows: &mut Vec<TreeRow>,
            restored_expansions: &mut HashSet<Vec<String>>,
        ) {
            let Some(issue_id) = path.last() else {
                return;
            };
            if !expanded.contains(path)
                && !expanded_issue_ids.is_some_and(|ids| ids.contains(issue_id))
            {
                return;
            }
            restored_expansions.insert(path.to_vec());
            let children = graph.tree_children(issue_id);
            let count = children.len();
            for (index, child_id) in children.iter().enumerate() {
                let last = index + 1 == count;
                let cycle = path.iter().any(|id| id == child_id);
                let mut child_path = path.to_vec();
                child_path.push(child_id.clone());
                rows.push(TreeRow {
                    issue_id: child_id.clone(),
                    path: child_path.clone(),
                    prefix: format!("{prefix}{}", if last { "└── " } else { "├── " }),
                    cycle,
                });
                if !cycle {
                    walk(
                        graph,
                        expanded,
                        expanded_issue_ids,
                        &child_path,
                        &format!("{prefix}{}", if last { "    " } else { "│   " }),
                        rows,
                        restored_expansions,
                    );
                }
            }
        }

        let mut rows = vec![TreeRow::create_entry()];
        let mut restored_expansions = HashSet::new();
        for root in self.graph.roots() {
            let path = vec![root.clone()];
            rows.push(TreeRow {
                issue_id: root.clone(),
                path: path.clone(),
                prefix: String::new(),
                cycle: false,
            });
            walk(
                &self.graph,
                &self.expanded,
                expanded_issue_ids,
                &path,
                "",
                &mut rows,
                &mut restored_expansions,
            );
        }
        (rows, restored_expansions)
    }

    fn rebuild_rows(&mut self) {
        let selected_path = self.rows.get(self.cursor).map(|row| row.path.clone());
        let (rows, _) = self.build_rows(None);
        self.rows = rows;
        self.tree_rows.clone_from(&self.rows);

        if let Some(path) = selected_path {
            if let Some(position) = self.rows.iter().position(|row| row.path == path) {
                self.cursor = position;
            }
        }
        self.cursor = self.cursor.min(self.rows.len().saturating_sub(1));
    }
}

/// Handles a key in the add-issue step. The flag is true when the step ends.
/// A create keeps the step open, so a failed create can be retried.
fn handle_add_issue_key(flow: &mut AddIssueFlow, key: KeyEvent, plain: bool) -> (Action, bool) {
    if flow.confirming_cancel {
        match key.code {
            KeyCode::Char('y' | 'Y') => return (Action::None, true),
            KeyCode::Char('n' | 'N') | KeyCode::Esc => flow.confirming_cancel = false,
            _ => {}
        }
        return (Action::None, false);
    }

    if key.code == KeyCode::Esc {
        flow.confirming_cancel = true;
        return (Action::None, false);
    }

    match flow.step {
        AddIssueStep::Title => match key.code {
            KeyCode::Enter if !flow.title.trim().is_empty() => {
                flow.step = AddIssueStep::Description;
            }
            KeyCode::Backspace => {
                flow.title.pop();
            }
            KeyCode::Char('e') if plain && flow.title.is_empty() => {
                return (Action::EditAddIssue(AddIssueField::Title), false);
            }
            KeyCode::Char(character) if plain => flow.title.push(character),
            _ => {}
        },
        AddIssueStep::Description => match key.code {
            KeyCode::Enter => flow.step = AddIssueStep::IssueType,
            KeyCode::Backspace => {
                flow.description.pop();
            }
            KeyCode::Char('e') if plain && flow.description.is_empty() => {
                return (Action::EditAddIssue(AddIssueField::Description), false);
            }
            KeyCode::Char(character) if plain => flow.description.push(character),
            _ => {}
        },
        AddIssueStep::IssueType => match key.code {
            KeyCode::Enter => flow.step = AddIssueStep::Priority,
            KeyCode::Backspace => flow.step = AddIssueStep::Description,
            code => move_selection(&mut flow.issue_type_index, ISSUE_TYPES.len(), code),
        },
        AddIssueStep::Priority => match key.code {
            KeyCode::Enter => return (Action::CreateIssue(flow.draft()), false),
            KeyCode::Backspace => flow.step = AddIssueStep::IssueType,
            code => move_selection(&mut flow.priority_index, PRIORITIES.len(), code),
        },
    }
    (Action::None, false)
}

/// The command `a` opens in `$EDITOR`. `/ship` takes the short ID: the
/// part after the workspace prefix (`2ybw.118` for `lifeline-2ybw.118`).
fn agent_command(issue_id: &str) -> String {
    let short_id = issue_id
        .rsplit_once('-')
        .map_or(issue_id, |(_, short_id)| short_id);
    format!("claude --effort medium \"/ship {short_id}\"")
}

/// The option index of `issue`'s current value, if it is one of `options`.
fn current_choice_index(kind: ChoiceKind, issue: &Issue, options: &[String]) -> Option<usize> {
    match kind {
        ChoiceKind::Status => options.iter().position(|status| *status == issue.status),
        ChoiceKind::Priority => PRIORITIES
            .iter()
            .position(|priority| *priority == issue.priority),
    }
}

/// Moves a list selection with j/k/g/G or the arrow, Home and End keys.
fn move_selection(index: &mut usize, len: usize, code: KeyCode) {
    let Some(last) = len.checked_sub(1) else {
        return;
    };
    match code {
        KeyCode::Char('j') | KeyCode::Down => *index = (*index + 1).min(last),
        KeyCode::Char('k') | KeyCode::Up => *index = index.saturating_sub(1),
        KeyCode::Char('g') | KeyCode::Home => *index = 0,
        KeyCode::Char('G') | KeyCode::End => *index = last,
        _ => {}
    }
}

fn fuzzy_match(candidate: &str, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }

    let mut search_from = 0;
    let candidate = candidate.to_lowercase();
    let query = query.to_lowercase();
    for needle in query.chars() {
        let Some(remainder) = candidate.get(search_from..) else {
            return false;
        };
        let Some(offset) = remainder.find(needle) else {
            return false;
        };
        search_from += offset + needle.len_utf8();
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Dependency, Issue};

    fn dependency(id: &str, kind: &str) -> Dependency {
        Dependency {
            id: id.into(),
            title: id.to_uppercase(),
            dependency_type: kind.into(),
            ..Dependency::default()
        }
    }

    /// An issue nested under `parent` in the tree.
    fn child(id: &str, parent: &str) -> Issue {
        Issue {
            id: id.into(),
            title: id.to_uppercase(),
            dependencies: vec![dependency(parent, "parent-child")],
            ..Issue::default()
        }
    }

    fn plain(id: &str) -> Issue {
        Issue {
            id: id.into(),
            title: id.to_uppercase(),
            ..Issue::default()
        }
    }

    /// `a` with child `b`; `a` also lists `b` as a blocker, so Task View
    /// has a dependency to open.
    fn graph() -> IssueGraph {
        let mut a = plain("a");
        a.dependencies.push(dependency("b", "blocks"));
        IssueGraph::new(vec![a, child("b", "a")], vec![])
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn type_text(app: &mut App, text: &str) {
        for character in text.chars() {
            app.handle_key(key(KeyCode::Char(character)));
        }
    }

    /// The tree rows without the synthetic create entry.
    fn issue_ids(app: &App) -> Vec<&str> {
        app.rows
            .iter()
            .filter(|row| !row.is_create_entry())
            .map(|row| row.issue_id.as_str())
            .collect()
    }

    fn choice(app: &App) -> &Choice {
        match app.step() {
            Some(Step::Choose(choice)) => choice,
            step => panic!("expected a choice step, got {step:?}"),
        }
    }

    #[test]
    fn tree_starts_at_first_level_and_expands_like_treec() {
        let mut app = App::new(graph());
        assert!(app.rows[0].is_create_entry());
        assert_eq!(issue_ids(&app), ["a"]);
        assert_eq!(app.cursor, 1, "cursor starts on the first issue");

        app.handle_key(key(KeyCode::Char('l')));
        assert_eq!(issue_ids(&app), ["a", "b"]);

        app.handle_key(key(KeyCode::Char('l')));
        assert_eq!(app.cursor, 2);
        app.handle_key(key(KeyCode::Char('h')));
        assert_eq!(app.cursor, 1);
    }

    #[test]
    fn parent_child_edges_expand_from_parent_to_child() {
        let mut app = App::new(IssueGraph::new(
            vec![child("hmb2", "8gda"), plain("8gda")],
            vec![],
        ));

        assert_eq!(issue_ids(&app), ["8gda"]);

        app.handle_key(key(KeyCode::Char('l')));
        assert_eq!(issue_ids(&app), ["8gda", "hmb2"]);
    }

    #[test]
    fn blocking_links_do_not_nest_in_the_tree() {
        let mut blocked = plain("later");
        blocked.dependencies.push(dependency("first", "blocks"));
        let mut app = App::new(IssueGraph::new(vec![blocked, plain("first")], vec![]));

        app.handle_key(key(KeyCode::Char('l')));
        assert_eq!(issue_ids(&app), ["later", "first"]);
        assert!(!app.row_has_children(&app.rows[1]));
    }

    #[test]
    fn task_view_uses_history_and_escape_returns_to_tree() {
        let mut app = App::new(graph());
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.screen(), Screen::Detail);
        assert_eq!(app.current_detail_issue().unwrap().id, "a");

        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.current_detail_issue().unwrap().id, "b");
        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(app.current_detail_issue().unwrap().id, "a");
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.screen(), Screen::Tree);
    }

    #[test]
    fn plus_starts_add_issue_at_the_selected_tree_location() {
        let mut app = App::new(graph());

        assert_eq!(app.handle_key(key(KeyCode::Char('+'))), Action::None);
        let flow = app.add_issue_flow().unwrap();
        assert_eq!(flow.parent_id.as_deref(), Some("a"));
        assert_eq!(flow.step, AddIssueStep::Title);
        assert_eq!(flow.issue_type(), "task");
        assert_eq!(flow.priority(), 1);
    }

    #[test]
    fn plus_starts_add_issue_at_the_current_task_view_location() {
        let mut app = App::new(graph());
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(key(KeyCode::Enter));

        app.handle_key(key(KeyCode::Char('+')));

        assert_eq!(
            app.add_issue_flow().unwrap().parent_id.as_deref(),
            Some("b")
        );
    }

    #[test]
    fn add_issue_flow_collects_fields_and_emits_a_create_action() {
        let mut app = App::new(graph());
        app.handle_key(key(KeyCode::Char('+')));

        assert_eq!(
            app.handle_key(key(KeyCode::Char('e'))),
            Action::EditAddIssue(AddIssueField::Title)
        );
        app.set_add_issue_field(AddIssueField::Title, "A new child\n".into());
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            app.add_issue_flow().unwrap().step,
            AddIssueStep::Description
        );
        assert_eq!(
            app.handle_key(key(KeyCode::Char('e'))),
            Action::EditAddIssue(AddIssueField::Description)
        );
        app.set_add_issue_field(AddIssueField::Description, "Some detail\n".into());
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Enter));

        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Action::CreateIssue(AddIssueDraft {
                parent_id: Some("a".into()),
                title: "A new child".into(),
                description: "Some detail".into(),
                issue_type: "bug".into(),
                priority: 1,
            })
        );
        assert!(
            app.add_issue_flow().is_some(),
            "failed creates can be retried"
        );
    }

    #[test]
    fn escape_confirms_before_discarding_add_issue_progress() {
        let mut app = App::new(graph());
        app.handle_key(key(KeyCode::Char('+')));
        app.handle_key(key(KeyCode::Char('F')));

        app.handle_key(key(KeyCode::Esc));
        assert!(app
            .add_issue_flow()
            .is_some_and(AddIssueFlow::is_confirming_cancel));
        app.handle_key(key(KeyCode::Esc));
        assert!(!app
            .add_issue_flow()
            .is_some_and(AddIssueFlow::is_confirming_cancel));
        assert_eq!(app.add_issue_flow().unwrap().title, "F");

        app.handle_key(key(KeyCode::Esc));
        app.handle_key(key(KeyCode::Char('y')));
        assert!(app.add_issue_flow().is_none());
        assert_eq!(app.screen(), Screen::Tree);
    }

    #[test]
    fn e_in_task_view_requests_description_edit_after_the_sequence_timeout() {
        let mut app = App::new(graph());
        app.handle_key(key(KeyCode::Enter));

        assert_eq!(app.handle_key(key(KeyCode::Char('e'))), Action::None);
        assert_eq!(
            app.flush_pending_key(),
            Action::Edit(EditField::Description)
        );
        assert_eq!(app.current_detail_issue().unwrap().id, "a");
    }

    #[test]
    fn e_in_tree_requests_description_edit_without_leaving_the_tree() {
        let mut app = App::new(graph());

        assert_eq!(app.handle_key(key(KeyCode::Char('e'))), Action::None);
        assert_eq!(
            app.flush_pending_key(),
            Action::Edit(EditField::Description)
        );
        assert_eq!(app.screen(), Screen::Tree);
        assert_eq!(app.current_issue().unwrap().id, "a");
    }

    #[test]
    fn e_then_t_in_tree_requests_title_edit_like_the_task_view() {
        let mut app = App::new(graph());

        assert_eq!(app.handle_key(key(KeyCode::Char('e'))), Action::None);
        assert_eq!(
            app.handle_key(key(KeyCode::Char('t'))),
            Action::Edit(EditField::Title)
        );
        assert_eq!(app.screen(), Screen::Tree);
        assert_eq!(app.current_issue().unwrap().id, "a");
    }

    #[test]
    fn e_then_t_in_task_view_requests_title_edit() {
        let mut app = App::new(graph());
        app.handle_key(key(KeyCode::Enter));

        assert_eq!(app.handle_key(key(KeyCode::Char('e'))), Action::None);
        assert_eq!(
            app.handle_key(key(KeyCode::Char('t'))),
            Action::Edit(EditField::Title)
        );
        assert_eq!(app.current_detail_issue().unwrap().id, "a");
    }

    #[test]
    fn e_then_an_unrelated_key_cancels_the_sequence_and_handles_the_key() {
        let mut app = App::new(graph());
        app.handle_key(key(KeyCode::Enter));

        assert_eq!(app.handle_key(key(KeyCode::Char('e'))), Action::None);
        assert_eq!(app.handle_key(key(KeyCode::Char('q'))), Action::Quit);
        assert_eq!(app.flush_pending_key(), Action::None);
    }

    #[test]
    fn e_pressed_twice_requests_description_edit_immediately() {
        let mut app = App::new(graph());
        app.handle_key(key(KeyCode::Enter));

        assert_eq!(app.handle_key(key(KeyCode::Char('e'))), Action::None);
        assert_eq!(
            app.handle_key(key(KeyCode::Char('e'))),
            Action::Edit(EditField::Description)
        );
    }

    #[test]
    fn a_refresh_does_not_cancel_a_pending_edit_chord() {
        let mut app = App::new(graph());
        app.handle_key(key(KeyCode::Char('e')));

        app.refresh_graph(graph());

        assert_eq!(
            app.handle_key(key(KeyCode::Char('t'))),
            Action::Edit(EditField::Title)
        );
    }

    #[test]
    fn the_busy_text_survives_keys_and_yields_to_a_status_message() {
        let mut app = App::new(graph());
        app.set_busy(Some("Closing a…".to_string()));

        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.status_message(), Some("Closing a…"));
        app.set_status("Set a to P1".to_string());
        assert_eq!(app.status_message(), Some("Set a to P1"));

        app.set_busy(None);
        app.handle_key(key(KeyCode::Char('k')));
        assert_eq!(app.status_message(), None);
    }

    #[test]
    fn status_message_clears_on_the_next_key() {
        let mut app = App::new(graph());
        app.set_status("Closing a…".to_string());
        assert_eq!(app.status_message(), Some("Closing a…"));

        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.status_message(), None);
    }

    #[test]
    fn refresh_forgets_expansion_of_issues_that_left_the_graph() {
        let mut app = App::new(graph());
        app.handle_key(key(KeyCode::Char('l')));
        assert_eq!(issue_ids(&app), ["a", "b"]);

        app.refresh_graph(IssueGraph::new(vec![plain("b")], vec![]));
        app.refresh_graph(graph());

        assert_eq!(issue_ids(&app), ["a"], "a must come back collapsed");
        assert!(!app.row_is_expanded(&app.rows[1]));
    }

    #[test]
    fn x_in_task_view_requires_confirmation_before_closing() {
        let mut app = App::new(graph());
        app.handle_key(key(KeyCode::Enter));

        assert_eq!(app.handle_key(key(KeyCode::Char('x'))), Action::None);
        assert!(app.is_confirming_close());
        assert_eq!(app.closing_issue_id(), Some("a"));
        assert_eq!(
            app.handle_key(key(KeyCode::Char('y'))),
            Action::CloseIssue("a".into())
        );
        assert!(!app.is_confirming_close());
        assert_eq!(app.current_detail_issue().unwrap().id, "a");
    }

    #[test]
    fn x_in_tree_view_closes_the_selected_issue_after_confirmation() {
        let mut app = App::new(graph());

        assert_eq!(app.handle_key(key(KeyCode::Char('x'))), Action::None);
        assert_eq!(app.closing_issue_id(), Some("a"));
        assert_eq!(
            app.handle_key(key(KeyCode::Char('y'))),
            Action::CloseIssue("a".into())
        );
        assert_eq!(app.screen(), Screen::Tree);
    }

    #[test]
    fn close_confirmation_can_be_cancelled() {
        let mut app = App::new(graph());
        app.handle_key(key(KeyCode::Enter));

        app.handle_key(key(KeyCode::Char('x')));
        assert_eq!(app.handle_key(key(KeyCode::Esc)), Action::None);
        assert!(!app.is_confirming_close());
        assert_eq!(app.screen(), Screen::Detail);
    }

    #[test]
    fn context_issues_cannot_be_closed_again() {
        let mut open = plain("open");
        open.dependencies.push(Dependency {
            status: "closed".into(),
            ..dependency("closed", "blocks")
        });
        let mut closed = plain("closed");
        closed.status = "closed".into();
        let mut app = App::new(IssueGraph::new(vec![open], vec![closed]));
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(key(KeyCode::Enter));

        assert!(!app.can_close_current_issue());
        assert_eq!(app.handle_key(key(KeyCode::Char('x'))), Action::None);
        assert!(!app.is_confirming_close());
    }

    #[test]
    fn refresh_preserves_tree_state_when_closing_reparents_dependencies() {
        let mut app = App::new(IssueGraph::new(
            vec![plain("a"), child("b", "a"), child("c", "b")],
            vec![],
        ));
        app.handle_key(key(KeyCode::Char('l')));
        app.handle_key(key(KeyCode::Char('j')));
        app.handle_key(key(KeyCode::Char('l')));
        assert_eq!(issue_ids(&app), ["a", "b", "c"]);

        app.cursor = 1;
        app.scroll = 0;
        app.handle_key(key(KeyCode::Char('x')));
        assert_eq!(
            app.handle_key(key(KeyCode::Char('y'))),
            Action::CloseIssue("a".into())
        );

        // `bd` removes closed a from the open list, making b a root. Its expanded
        // state follows the issue identity so c stays visible on the moved branch.
        app.return_to_tree();
        app.refresh_graph(IssueGraph::new(
            vec![child("b", "a"), child("c", "b")],
            vec![],
        ));

        assert_eq!(issue_ids(&app), ["b", "c"]);
        assert_eq!(app.current_tree_issue().unwrap().id, "b");
        assert!(app.row_is_expanded(&app.rows[1]));
        assert_eq!(app.scroll, 0);
    }

    #[test]
    fn refresh_keeps_path_specific_expansion_when_topology_is_unchanged() {
        let shared_graph = || {
            let mut shared = child("c", "a");
            shared.dependencies.push(dependency("d", "parent-child"));
            IssueGraph::new(
                vec![plain("a"), plain("d"), shared, child("e", "c")],
                vec![],
            )
        };
        let mut app = App::new(shared_graph());
        app.handle_key(key(KeyCode::Char('l'))); // expand a
        app.handle_key(key(KeyCode::Char('j')));
        app.handle_key(key(KeyCode::Char('l'))); // expand c under a
        app.cursor = 4;
        app.handle_key(key(KeyCode::Char('l'))); // expand d, but not c under d

        app.refresh_graph(shared_graph());

        assert_eq!(issue_ids(&app), ["a", "c", "e", "d", "c"]);
        assert_eq!(app.current_tree_issue().unwrap().id, "d");
    }

    #[test]
    fn dependencies_outside_the_filtered_list_stay_out_of_the_tree() {
        let mut open = plain("open");
        open.dependencies.push(Dependency {
            status: "closed".into(),
            ..dependency("closed", "parent-child")
        });
        let mut app = App::new(IssueGraph::new(vec![open], vec![]));

        app.handle_key(key(KeyCode::Char('l')));
        assert_eq!(issue_ids(&app), ["open"]);

        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.selected_dependency().unwrap().id, "closed");
    }

    #[test]
    fn slash_fuzzy_filters_visible_ids_and_enter_opens_match() {
        let mut app = App::new(IssueGraph::new(
            vec![plain("issue-alpha"), plain("issue-jbeta")],
            vec![],
        ));

        app.handle_key(key(KeyCode::Char('/')));
        type_text(&mut app, "jbt");
        assert_eq!(app.rows.len(), 1);
        assert_eq!(app.rows[0].issue_id, "issue-jbeta");

        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.search_query(), None);
        assert_eq!(app.current_detail_issue().unwrap().id, "issue-jbeta");
    }

    #[test]
    fn escape_cancels_search_and_restores_tree_selection() {
        let mut app = App::new(graph());
        app.handle_key(key(KeyCode::Char('/')));
        app.handle_key(key(KeyCode::Char('z')));
        assert!(app.rows.is_empty());

        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.search_query(), None);
        assert_eq!(app.current_tree_issue().unwrap().id, "a");
    }

    #[test]
    fn a_refresh_keeps_the_search_and_its_selection() {
        let mut app = App::new(IssueGraph::new(
            vec![plain("x-1"), plain("x-2"), plain("y-1")],
            vec![],
        ));
        app.handle_key(key(KeyCode::Char('/')));
        app.handle_key(key(KeyCode::Char('x')));
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.rows[app.cursor].issue_id, "x-2");

        app.refresh_graph(IssueGraph::new(
            vec![plain("x-0"), plain("x-1"), plain("x-2"), plain("y-1")],
            vec![],
        ));

        assert_eq!(app.search_query(), Some("x"));
        assert_eq!(issue_ids(&app), ["x-0", "x-1", "x-2"]);
        assert_eq!(app.rows[app.cursor].issue_id, "x-2");
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.current_tree_issue().unwrap().id, "x-1");
    }

    #[test]
    fn enter_on_the_create_entry_starts_a_top_level_add_issue() {
        let mut app = App::new(graph());
        app.handle_key(key(KeyCode::Char('g')));
        assert!(app.rows[app.cursor].is_create_entry());

        app.handle_key(key(KeyCode::Enter));

        let flow = app.add_issue_flow().unwrap();
        assert_eq!(flow.parent_id, None);
        assert_eq!(flow.step, AddIssueStep::Title);
    }

    #[test]
    fn create_entry_is_available_even_when_the_tree_is_empty() {
        let mut app = App::new(IssueGraph::new(vec![], vec![]));
        assert_eq!(app.rows.len(), 1);
        assert!(app.rows[0].is_create_entry());

        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.add_issue_flow().unwrap().parent_id, None);
    }

    #[test]
    fn create_entry_ignores_issue_keys_and_stays_out_of_search() {
        let mut app = App::new(graph());
        app.set_tmux(true);
        app.handle_key(key(KeyCode::Char('g')));

        assert_eq!(app.handle_key(key(KeyCode::Char('x'))), Action::None);
        assert!(!app.is_confirming_close());
        assert_eq!(app.handle_key(key(KeyCode::Char('e'))), Action::None);
        assert_eq!(app.flush_pending_key(), Action::None);
        app.handle_key(key(KeyCode::Char('s')));
        app.handle_key(key(KeyCode::Char('a')));
        assert!(app.step().is_none());

        app.handle_key(key(KeyCode::Char('/')));
        app.handle_key(key(KeyCode::Char('c')));
        assert!(app.rows.is_empty(), "'c' must not match the create entry");
    }

    #[test]
    fn s_opens_a_status_step_preselecting_the_current_status() {
        let mut working = plain("a");
        working.status = "in_progress".into();
        let mut app = App::new(IssueGraph::new(vec![working], vec![]));

        app.handle_key(key(KeyCode::Char('s')));
        let step = choice(&app);
        assert_eq!(step.kind, ChoiceKind::Status);
        assert_eq!(step.issue_id, "a");
        assert_eq!(step.options[step.index], "in_progress");

        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Action::SetStatus("a".into(), "blocked".into())
        );
        assert!(app.step().is_none());
        assert_eq!(app.screen(), Screen::Tree);
    }

    #[test]
    fn the_status_step_offers_the_statuses_bd_reports() {
        let mut app = App::new(graph());
        app.set_statuses(vec!["open".into(), "review".into()]);

        app.handle_key(key(KeyCode::Char('s')));
        assert_eq!(choice(&app).options, ["open", "review"]);
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Action::SetStatus("a".into(), "review".into())
        );
    }

    #[test]
    fn p_opens_a_priority_step_in_the_task_view_too() {
        let mut app = App::new(graph());
        app.handle_key(key(KeyCode::Enter));

        app.handle_key(key(KeyCode::Char('p')));
        let step = choice(&app);
        assert_eq!(step.kind, ChoiceKind::Priority);
        assert_eq!(step.options[step.index], "P0");

        app.handle_key(key(KeyCode::Char('j')));
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Action::SetPriority("a".into(), 2)
        );
        assert_eq!(
            app.screen(),
            Screen::Detail,
            "the step returns to task view"
        );
    }

    #[test]
    fn escape_cancels_a_choice_without_an_action() {
        let mut app = App::new(graph());
        app.handle_key(key(KeyCode::Char('s')));
        assert!(app.step().is_some());

        assert_eq!(app.handle_key(key(KeyCode::Esc)), Action::None);
        assert!(app.step().is_none());
        assert_eq!(app.screen(), Screen::Tree);
    }

    #[test]
    fn confirming_the_preselected_value_ends_the_step_without_an_update() {
        let mut app = App::new(graph());
        app.handle_key(key(KeyCode::Char('p')));

        assert_eq!(app.handle_key(key(KeyCode::Enter)), Action::None);
        assert!(app.step().is_none());
    }

    #[test]
    fn a_refresh_keeps_an_open_step() {
        let mut app = App::new(graph());
        app.handle_key(key(KeyCode::Char('s')));
        app.handle_key(key(KeyCode::Char('j')));

        app.refresh_graph(graph());

        assert_eq!(choice(&app).index, 1);
    }

    #[test]
    fn a_step_ends_when_its_issue_is_deleted_or_closed_elsewhere() {
        let mut app = App::new(graph());
        app.handle_key(key(KeyCode::Char('s')));
        app.refresh_graph(IssueGraph::new(vec![plain("b")], vec![]));
        assert!(app.step().is_none());
        assert_eq!(app.status_message(), Some("a no longer exists"));

        let mut app = App::new(graph());
        app.handle_key(key(KeyCode::Char('x')));
        let mut closed = plain("a");
        closed.status = "closed".into();
        app.refresh_graph(IssueGraph::new(vec![plain("b")], vec![closed]));
        assert!(app.step().is_none());
        assert_eq!(app.status_message(), Some("a is no longer open"));
    }

    #[test]
    fn search_only_includes_rows_visible_before_search() {
        let mut app = App::new(graph());

        app.handle_key(key(KeyCode::Char('/')));
        app.handle_key(key(KeyCode::Char('b')));
        assert!(
            app.rows.is_empty(),
            "collapsed child must not be searchable"
        );

        app.handle_key(key(KeyCode::Esc));
        app.handle_key(key(KeyCode::Char('l')));
        app.handle_key(key(KeyCode::Char('/')));
        app.handle_key(key(KeyCode::Char('b')));
        assert_eq!(app.rows.len(), 1);
        assert_eq!(app.rows[0].issue_id, "b");
        assert_eq!(app.rows[0].prefix, "└── ");
    }

    #[test]
    fn a_offers_a_ship_command_for_the_short_id() {
        let mut app = App::new(IssueGraph::new(vec![plain("lifeline-2ybw.118")], vec![]));
        app.set_tmux(true);

        assert_eq!(
            app.handle_key(key(KeyCode::Char('a'))),
            Action::StartAgent {
                issue_id: "lifeline-2ybw.118".into(),
                command: "claude --effort medium \"/ship 2ybw.118\"".into(),
            }
        );
        assert!(app.step().is_none());
    }

    #[test]
    fn agent_keys_need_tmux() {
        let mut app = App::new(graph());

        app.handle_key(key(KeyCode::Char('a')));
        assert!(app.step().is_none());
        assert!(app.status_message().unwrap().contains("tmux"));

        assert_eq!(app.handle_key(key(KeyCode::Char('f'))), Action::None);
        assert!(app.status_message().unwrap().contains("tmux"));
    }

    #[test]
    fn f_focuses_the_bound_pane_and_never_creates_one() {
        let mut app = App::new(graph());
        app.set_tmux(true);

        assert_eq!(app.handle_key(key(KeyCode::Char('f'))), Action::None);
        assert_eq!(app.status_message(), Some("no agent pane for a"));
        assert!(app.step().is_none());

        assert!(app.set_panes(HashMap::from([("a".to_string(), "%7".to_string())])));
        assert!(!app.set_panes(HashMap::from([("a".to_string(), "%7".to_string())])));
        assert_eq!(
            app.handle_key(key(KeyCode::Char('f'))),
            Action::FocusAgent("%7".into())
        );
    }

    #[test]
    fn paste_goes_into_the_text_being_edited() {
        let mut app = App::new(IssueGraph::new(vec![plain("x-1"), plain("y-1")], vec![]));
        app.handle_key(key(KeyCode::Char('/')));
        app.handle_paste("y-");
        assert_eq!(issue_ids(&app), ["y-1"]);
        app.handle_key(key(KeyCode::Esc));

        app.handle_key(key(KeyCode::Char('+')));
        app.handle_paste("Two\nlines");
        assert_eq!(app.add_issue_flow().unwrap().title, "Two lines");
        app.handle_key(key(KeyCode::Enter));
        app.handle_paste("Body\nkeeps lines");
        assert_eq!(
            app.add_issue_flow().unwrap().description,
            "Body\nkeeps lines"
        );
    }
}
