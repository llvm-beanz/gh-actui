use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    env, fs, io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        mpsc::{self, Receiver, TryRecvError},
    },
    thread,
    time::{Duration, Instant},
};

use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{LeaveAlternateScreen, disable_raw_mode},
};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Tabs, Wrap},
};

use crate::{
    github::{
        MAX_CONCURRENT_REQUESTS, RunCounts, RunStatus, Workflow, WorkflowSource, WorkflowTriage,
    },
    query::{FilterExpression, SortSpec, select_workflows},
    repository::Repository,
    state::{EQUAL_SPLIT_RATIO, SplitDirection, ViewLayout, ViewPane, ViewState, ViewTab},
};

const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(250);
const STATUS_FLASH_INTERVAL: Duration = Duration::from_millis(500);
const DEFAULT_REFRESH_INTERVAL: Duration = Duration::from_secs(15);
const SPLIT_RATIO_SCALE: u16 = 1000;
const MIN_VIEW_HEIGHT: u16 = 5;
const MIN_VIEW_WIDTH: u16 = 20;
const MAX_COMMAND_HISTORY: usize = 1000;

#[derive(Debug, Default, Eq, PartialEq)]
enum Mode {
    #[default]
    Normal,
    Command,
}

#[derive(Clone, Copy)]
enum FocusDirection {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Clone, Copy)]
enum ResizeAxis {
    Height,
    Width,
}

#[derive(Clone, Copy, Debug)]
enum ResizeAmount {
    Absolute(u16),
    Delta(i16),
}

struct PendingLoad {
    repository: Repository,
    workflow_ids: Option<Vec<u64>>,
    state_path: Option<PathBuf>,
    kind: LoadKind,
    tabs: Option<Vec<ViewTab>>,
    active_tab: usize,
}

enum LoadEvent {
    Discovered(Vec<Workflow>),
    WorkflowLoaded(Workflow),
    WorkflowFailed {
        id: u64,
        name: String,
        error: String,
    },
    Fatal(String),
    Complete,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LoadKind {
    Initial,
    Edit,
    Refresh,
}

#[derive(Clone, Copy)]
enum TriageTarget {
    NewTab { insert_after: usize },
    RefreshTab { index: usize },
}

#[derive(Clone)]
struct ListView {
    workflow_ids: Vec<u64>,
    table_state: TableState,
    filter: Option<String>,
    filter_expression: Option<FilterExpression>,
    sort: Option<String>,
    sort_spec: Option<SortSpec>,
}

impl ListView {
    fn new(workflow_ids: Vec<u64>, filter: Option<String>, sort: Option<String>) -> Self {
        Self {
            table_state: TableState::default(),
            filter_expression: filter
                .as_deref()
                .and_then(|expression| FilterExpression::parse(expression).ok()),
            sort_spec: sort
                .as_deref()
                .and_then(|specification| SortSpec::parse(specification).ok()),
            workflow_ids,
            filter,
            sort,
        }
    }

    fn from_persisted(view: ViewPane) -> Self {
        Self::new(view.workflow_ids, view.filter, view.sort)
    }
}

#[derive(Clone)]
struct Tab {
    name: Option<String>,
    views: Vec<ListView>,
    layout: ViewLayout,
    active_view: usize,
    is_triage: bool,
}

impl Tab {
    fn new(
        name: Option<String>,
        workflow_ids: Vec<u64>,
        filter: Option<String>,
        sort: Option<String>,
    ) -> Self {
        Self {
            name,
            views: vec![ListView::new(workflow_ids, filter, sort)],
            layout: ViewLayout::default(),
            active_view: 0,
            is_triage: false,
        }
    }

    fn from_view(tab: ViewTab) -> Self {
        let mut views = vec![ListView::new(tab.workflow_ids, tab.filter, tab.sort)];
        views.extend(
            tab.additional_views
                .into_iter()
                .map(ListView::from_persisted),
        );
        Self {
            name: tab.name,
            active_view: tab.active_view.min(views.len().saturating_sub(1)),
            views,
            layout: tab.layout,
            is_triage: false,
        }
    }

    fn active_view(&self) -> &ListView {
        &self.views[self.active_view]
    }

    fn active_view_mut(&mut self) -> &mut ListView {
        &mut self.views[self.active_view]
    }
}

impl std::ops::Deref for Tab {
    type Target = ListView;

    fn deref(&self) -> &Self::Target {
        self.active_view()
    }
}

impl std::ops::DerefMut for Tab {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.active_view_mut()
    }
}

struct App {
    repository: Repository,
    workflows: Vec<Workflow>,
    state_path: Option<PathBuf>,
    workflow_source: Arc<dyn WorkflowSource>,
    load_receiver: Option<Receiver<LoadEvent>>,
    pending_load: Option<PendingLoad>,
    load_failures: Vec<String>,
    loaded_status_count: usize,
    loading_status_ids: BTreeSet<u64>,
    triage_receiver: Option<Receiver<Result<Vec<WorkflowTriage>, crate::github::Error>>>,
    triage_target: Option<TriageTarget>,
    triage: HashMap<u64, WorkflowTriage>,
    tabs: Vec<Tab>,
    active_tab: usize,
    mode: Mode,
    command: String,
    command_cursor: usize,
    command_history: Vec<String>,
    command_history_path: Option<PathBuf>,
    history_index: Option<usize>,
    history_draft: String,
    message: Option<String>,
    pending_g: bool,
    should_quit: bool,
    animation_started: Instant,
    refresh_interval: Duration,
    next_refresh: Instant,
    pending_ctrl_w: bool,
    pane_areas: Vec<(usize, Rect)>,
}

impl App {
    fn new(
        repository: Repository,
        workflows: Vec<Workflow>,
        state_path: Option<PathBuf>,
        workflow_source: Arc<dyn WorkflowSource>,
        saved_tabs: Option<(Vec<ViewTab>, usize)>,
    ) -> Self {
        let (tabs, active_tab) = saved_tabs.map_or_else(
            || {
                (
                    vec![Tab::new(
                        None,
                        workflows.iter().map(|workflow| workflow.id).collect(),
                        None,
                        None,
                    )],
                    0,
                )
            },
            |(tabs, active_tab)| {
                let tabs = tabs.into_iter().map(Tab::from_view).collect::<Vec<_>>();
                let active_tab = active_tab.min(tabs.len().saturating_sub(1));
                (tabs, active_tab)
            },
        );
        let mut app = Self {
            repository,
            workflows,
            state_path,
            workflow_source,
            load_receiver: None,
            pending_load: None,
            load_failures: Vec::new(),
            loaded_status_count: 0,
            loading_status_ids: BTreeSet::new(),
            triage_receiver: None,
            triage_target: None,
            triage: HashMap::new(),
            tabs,
            active_tab,
            mode: Mode::Normal,
            command: String::new(),
            command_cursor: 0,
            command_history: Vec::new(),
            command_history_path: None,
            history_index: None,
            history_draft: String::new(),
            message: None,
            pending_g: false,
            should_quit: false,
            animation_started: Instant::now(),
            refresh_interval: DEFAULT_REFRESH_INTERVAL,
            next_refresh: Instant::now() + DEFAULT_REFRESH_INTERVAL,
            pending_ctrl_w: false,
            pane_areas: Vec::new(),
        };
        app.repair_all_tab_selections(None);
        app
    }

    fn new_loading(
        repository: Repository,
        workflow_ids: Option<Vec<u64>>,
        state_path: Option<PathBuf>,
        workflow_source: Arc<dyn WorkflowSource>,
        tabs: Option<Vec<ViewTab>>,
        active_tab: usize,
    ) -> Self {
        let mut app = Self::new(repository.clone(), Vec::new(), None, workflow_source, None);
        app.start_loading(PendingLoad {
            repository,
            workflow_ids,
            state_path,
            kind: LoadKind::Initial,
            tabs,
            active_tab,
        });
        app
    }

    fn run(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        while !self.should_quit {
            self.poll_loading();
            self.poll_triage();
            self.refresh_if_due();
            terminal.draw(|frame| self.render(frame))?;

            if event::poll(EVENT_POLL_INTERVAL)? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => self.handle_key(key),
                    Event::Mouse(mouse) => self.handle_mouse(mouse),
                    _ => {}
                }
            }
        }

        Ok(())
    }

    fn start_loading(&mut self, pending: PendingLoad) {
        let source = Arc::clone(&self.workflow_source);
        let repository = pending.repository.clone();
        let wanted_ids: Option<BTreeSet<u64>> = pending
            .workflow_ids
            .as_ref()
            .map(|ids| ids.iter().copied().collect());
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let workflows = match source.list_workflows(&repository) {
                Ok(workflows) => workflows,
                Err(error) => {
                    let _ = sender.send(LoadEvent::Fatal(error.to_string()));
                    return;
                }
            };
            if sender
                .send(LoadEvent::Discovered(workflows.clone()))
                .is_err()
            {
                return;
            }
            let _ = &wanted_ids;
            let targets: Vec<Workflow> = workflows;
            {
                let _unused = |wanted_ids: BTreeSet<u64>, workflow: &Workflow| {
                    wanted_ids.contains(&workflow.id)
                };
            }
            for chunk in targets.chunks(MAX_CONCURRENT_REQUESTS) {
                thread::scope(|scope| {
                    for workflow in chunk {
                        let sender = sender.clone();
                        let source = Arc::clone(&source);
                        let repository = &repository;
                        scope.spawn(move || {
                            let event = match source.load_workflow_status(repository, workflow) {
                                Ok(workflow) => LoadEvent::WorkflowLoaded(workflow),
                                Err(error) => LoadEvent::WorkflowFailed {
                                    id: workflow.id,
                                    name: workflow.name.clone(),
                                    error: error.to_string(),
                                },
                            };
                            let _ = sender.send(event);
                        });
                    }
                });
            }
            let _ = sender.send(LoadEvent::Complete);
        });
        self.load_receiver = Some(receiver);
        self.pending_load = Some(pending);
        self.load_failures.clear();
        self.loaded_status_count = 0;
        self.loading_status_ids.clear();
        self.message = None;
    }

    fn poll_loading(&mut self) {
        loop {
            let Some(receiver) = self.load_receiver.as_ref() else {
                return;
            };
            let event = match receiver.try_recv() {
                Ok(event) => event,
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    self.finish_loading(Err("workflow loading stopped unexpectedly".to_owned()));
                    return;
                }
            };
            self.handle_load_event(event);
        }
    }

    fn handle_load_event(&mut self, event: LoadEvent) {
        match event {
            LoadEvent::Discovered(workflows) => self.apply_discovered_workflows(workflows),
            LoadEvent::WorkflowLoaded(workflow) => {
                let selected_ids = self.selected_workflow_ids();
                self.loading_status_ids.remove(&workflow.id);
                if let Some(existing) = self
                    .workflows
                    .iter_mut()
                    .find(|existing| existing.id == workflow.id)
                {
                    *existing = workflow;
                    self.loaded_status_count += 1;
                }
                self.repair_all_tab_selections(Some(selected_ids));
            }
            LoadEvent::WorkflowFailed { id, name, error } => {
                self.loading_status_ids.remove(&id);
                if self.workflows.iter().any(|workflow| workflow.id == id) {
                    self.loaded_status_count += 1;
                }
                let failure = format!("{name}: {error}");
                self.message = Some(format!("E484: Could not load status for {failure}"));
                self.load_failures.push(failure);
            }
            LoadEvent::Fatal(error) => self.finish_loading(Err(error)),
            LoadEvent::Complete => self.finish_loading(Ok(())),
        }
    }

    fn poll_triage(&mut self) {
        let Some(receiver) = self.triage_receiver.as_ref() else {
            return;
        };
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                self.triage_receiver = None;
                self.triage_target = None;
                self.message = Some("E484: workflow triage stopped unexpectedly".to_owned());
                return;
            }
        };
        self.triage_receiver = None;
        let target = self.triage_target.take();
        self.finish_triage(result.map_err(|error| error.to_string()), target);
    }

    fn finish_triage(
        &mut self,
        result: Result<Vec<WorkflowTriage>, String>,
        target: Option<TriageTarget>,
    ) {
        match result {
            Ok(results) => {
                let workflow_ids = results
                    .iter()
                    .map(|result| result.workflow_id)
                    .collect::<Vec<_>>();
                let result_count = workflow_ids.len();
                self.triage.extend(
                    results
                        .into_iter()
                        .map(|result| (result.workflow_id, result)),
                );
                match target {
                    Some(TriageTarget::RefreshTab { index })
                        if self.tabs.get(index).is_some_and(|tab| tab.is_triage) =>
                    {
                        let selected_id = self.tabs[index]
                            .active_view()
                            .table_state
                            .selected()
                            .and_then(|selected| {
                                self.visible_workflows_for(self.tabs[index].active_view())
                                    .get(selected)
                                    .copied()
                            })
                            .map(|workflow| workflow.id);
                        self.tabs[index].active_view_mut().workflow_ids = workflow_ids;
                        let selected = selected_id
                            .and_then(|id| {
                                self.visible_workflows_for(self.tabs[index].active_view())
                                    .iter()
                                    .position(|workflow| workflow.id == id)
                            })
                            .or_else(|| {
                                (!self.tabs[index].active_view().workflow_ids.is_empty())
                                    .then_some(0)
                            });
                        self.tabs[index]
                            .active_view_mut()
                            .table_state
                            .select(selected);
                    }
                    Some(TriageTarget::NewTab { insert_after }) => {
                        let mut tab = Tab::new(Some("Triage".to_owned()), workflow_ids, None, None);
                        tab.is_triage = true;
                        let has_workflows = !tab.active_view().workflow_ids.is_empty();
                        tab.active_view_mut()
                            .table_state
                            .select(has_workflows.then_some(0));
                        let index = (insert_after + 1).min(self.tabs.len());
                        self.tabs.insert(index, tab);
                        self.active_tab = index;
                    }
                    _ => {
                        self.message =
                            Some("E484: triage destination is no longer available".to_owned());
                        return;
                    }
                }
                self.message = Some(format!(
                    "{} failing scheduled workflows triaged",
                    result_count
                ));
            }
            Err(error) => self.message = Some(format!("E484: {error}")),
        }
    }

    fn apply_discovered_workflows(&mut self, mut workflows: Vec<Workflow>) {
        let Some(pending) = self.pending_load.as_ref() else {
            return;
        };
        let selected_ids = self.selected_workflow_ids();
        let kind = pending.kind;
        let repository = pending.repository.clone();
        let workflow_ids = pending.workflow_ids.clone();
        let state_path = pending.state_path.clone();
        let saved_tabs = pending.tabs.clone();
        let saved_active_tab = pending.active_tab;

        let mut carried_over = BTreeSet::new();
        if kind == LoadKind::Refresh {
            for workflow in &mut workflows {
                if let Some(previous) = self
                    .workflows
                    .iter()
                    .find(|previous| previous.id == workflow.id)
                {
                    workflow.run_status = previous.run_status;
                    workflow.is_in_progress = previous.is_in_progress;
                    workflow.run_metrics = previous.run_metrics;
                    carried_over.insert(workflow.id);
                }
            }
        }
        self.repository = repository;
        self.workflows = match workflow_ids {
            Some(workflow_ids) => ViewState::resolve_workflows(&workflow_ids, workflows),
            None => workflows,
        };
        self.loading_status_ids = self
            .workflows
            .iter()
            .map(|workflow| workflow.id)
            .filter(|id| !carried_over.contains(id))
            .collect();
        self.loaded_status_count = 0;
        self.state_path = state_path;
        if kind != LoadKind::Refresh {
            self.tabs = saved_tabs.map_or_else(
                || {
                    vec![Tab::new(
                        None,
                        self.workflows.iter().map(|workflow| workflow.id).collect(),
                        None,
                        None,
                    )]
                },
                |tabs| {
                    tabs.into_iter()
                        .map(|tab| {
                            let selected_ids = std::iter::once(tab.selected_workflow_id)
                                .chain(
                                    tab.additional_views
                                        .iter()
                                        .map(|view| view.selected_workflow_id),
                                )
                                .collect::<Vec<_>>();
                            let mut runtime = Tab::from_view(tab);
                            for (view, selected_id) in runtime.views.iter_mut().zip(selected_ids) {
                                let selected = selected_id.and_then(|id| {
                                    self.visible_workflows_for(view)
                                        .iter()
                                        .position(|workflow| workflow.id == id)
                                });
                                view.table_state.select(selected);
                            }
                            runtime
                        })
                        .collect()
                },
            );
            self.active_tab = saved_active_tab.min(self.tabs.len().saturating_sub(1));
        }
        self.repair_all_tab_selections((kind == LoadKind::Refresh).then_some(selected_ids));
        self.message = Some(format!(
            "{} workflows discovered; loading run status...",
            self.workflows.len()
        ));
    }

    fn finish_loading(&mut self, result: Result<(), String>) {
        self.load_receiver = None;
        self.loading_status_ids.clear();
        let Some(pending) = self.pending_load.take() else {
            return;
        };

        match result {
            Ok(()) => {
                let completed = match pending.kind {
                    LoadKind::Refresh => format!("{} workflows refreshed", self.workflows.len()),
                    LoadKind::Initial | LoadKind::Edit => {
                        format!("{} workflows loaded", self.workflows.len())
                    }
                };
                self.message = Some(if self.load_failures.is_empty() {
                    completed
                } else {
                    format!(
                        "{completed}; {} workflow statuses failed",
                        self.load_failures.len()
                    )
                });
            }
            Err(error) => self.message = Some(format!("E484: {error}")),
        }
        self.next_refresh = Instant::now() + self.refresh_interval;
    }

    fn is_loading(&self) -> bool {
        self.pending_load.is_some()
    }

    fn visible_workflows(&self) -> Vec<&Workflow> {
        self.tabs.get(self.active_tab).map_or_else(Vec::new, |tab| {
            self.visible_workflows_for(tab.active_view())
        })
    }

    fn visible_workflows_for(&self, view: &ListView) -> Vec<&Workflow> {
        let mut workflows = select_workflows(
            &self.workflows,
            view.filter_expression.as_ref(),
            view.sort_spec.as_ref(),
        );
        workflows.retain(|workflow| view.workflow_ids.contains(&workflow.id));
        workflows
    }

    fn active_tab(&self) -> &Tab {
        &self.tabs[self.active_tab]
    }

    fn active_tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active_tab]
    }

    fn active_view(&self) -> &ListView {
        self.active_tab().active_view()
    }

    fn active_view_mut(&mut self) -> &mut ListView {
        self.active_tab_mut().active_view_mut()
    }

    fn selected_workflow_ids(&self) -> Vec<Vec<Option<u64>>> {
        self.tabs
            .iter()
            .map(|tab| {
                tab.views
                    .iter()
                    .map(|view| {
                        view.table_state
                            .selected()
                            .and_then(|index| self.visible_workflows_for(view).get(index).copied())
                            .map(|workflow| workflow.id)
                    })
                    .collect()
            })
            .collect()
    }

    fn repair_all_tab_selections(&mut self, selected_ids: Option<Vec<Vec<Option<u64>>>>) {
        let selections = self
            .tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| {
                tab.views
                    .iter()
                    .enumerate()
                    .map(|(view_index, view)| {
                        let visible = self.visible_workflows_for(view);
                        selected_ids
                            .as_ref()
                            .and_then(|ids| {
                                ids.get(index)
                                    .and_then(|views| views.get(view_index))
                                    .copied()
                                    .flatten()
                            })
                            .and_then(|id| visible.iter().position(|workflow| workflow.id == id))
                            .or_else(|| {
                                view.table_state
                                    .selected()
                                    .filter(|selected| *selected < visible.len())
                            })
                            .or_else(|| (!visible.is_empty()).then_some(0))
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        for (tab, tab_selections) in self.tabs.iter_mut().zip(selections) {
            for (view, selection) in tab.views.iter_mut().zip(tab_selections) {
                view.table_state.select(selection);
            }
        }
    }

    fn refresh_if_due(&mut self) {
        if !self.is_loading()
            && self.triage_receiver.is_none()
            && Instant::now() >= self.next_refresh
        {
            self.refresh_workflows();
        }
    }

    fn refresh(&mut self) {
        if self.active_tab().is_triage {
            self.refresh_triage();
        } else {
            self.refresh_workflows();
        }
    }

    fn refresh_workflows(&mut self) {
        if self.is_loading() || self.triage_receiver.is_some() {
            self.message = Some("Triage or refresh already in progress".to_owned());
            return;
        }

        self.start_loading(PendingLoad {
            repository: self.repository.clone(),
            workflow_ids: Some(self.workflows.iter().map(|workflow| workflow.id).collect()),
            state_path: self.state_path.clone(),
            kind: LoadKind::Refresh,
            tabs: None,
            active_tab: self.active_tab,
        });
    }

    fn refresh_rate_label(&self) -> String {
        format!("refresh: {}s", self.refresh_interval.as_secs())
    }

    fn normal_status(&self) -> (&'static str, String) {
        if self.triage_receiver.is_some() {
            (
                " TRIAGING ",
                "Analyzing scheduled failures, jobs, steps, and logs...".to_owned(),
            )
        } else if let Some(pending) = self.pending_load.as_ref() {
            let action = match pending.kind {
                LoadKind::Refresh => "Refreshing",
                LoadKind::Initial | LoadKind::Edit => "Loading",
            };
            (
                if pending.kind == LoadKind::Refresh {
                    " REFRESHING "
                } else {
                    " LOADING "
                },
                if self.workflows.is_empty() {
                    format!("{action} workflows for {}...", pending.repository)
                } else {
                    format!(
                        "{action} run status for {}... ({}/{})",
                        pending.repository,
                        self.loaded_status_count,
                        self.workflows.len()
                    )
                },
            )
        } else {
            (
                " NORMAL ",
                self.message
                    .as_deref()
                    .unwrap_or(
                        "j/k: select  |  Ctrl-d/Ctrl-u: scroll  |  Ctrl-Tab: next tab  |  :q: quit",
                    )
                    .to_owned(),
            )
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => {
                    self.tab_previous();
                    return;
                }
                KeyCode::Tab => {
                    self.tab_next();
                    return;
                }
                KeyCode::BackTab => {
                    self.tab_previous();
                    return;
                }
                _ => {}
            }
        }

        match self.mode {
            Mode::Normal => self.handle_normal_key(key),
            Mode::Command => self.handle_command_key(key),
        }
    }

    fn handle_normal_key(&mut self, key: KeyEvent) {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);

        if self.pending_ctrl_w {
            self.pending_ctrl_w = false;
            match key.code {
                KeyCode::Left | KeyCode::Char('h') => self.move_view_focus(FocusDirection::Left),
                KeyCode::Down | KeyCode::Char('j') => self.move_view_focus(FocusDirection::Down),
                KeyCode::Up | KeyCode::Char('k') => self.move_view_focus(FocusDirection::Up),
                KeyCode::Right | KeyCode::Char('l') => self.move_view_focus(FocusDirection::Right),
                KeyCode::Char('+') => {
                    self.resize_active_view(ResizeAxis::Height, ResizeAmount::Delta(1))
                }
                KeyCode::Char('-') => {
                    self.resize_active_view(ResizeAxis::Height, ResizeAmount::Delta(-1))
                }
                KeyCode::Char('>') => {
                    self.resize_active_view(ResizeAxis::Width, ResizeAmount::Delta(1))
                }
                KeyCode::Char('<') => {
                    self.resize_active_view(ResizeAxis::Width, ResizeAmount::Delta(-1))
                }
                KeyCode::Char('=') => self.equalize_splits(),
                _ => {}
            }
            return;
        }

        match key.code {
            KeyCode::Char('w') if control => {
                self.pending_ctrl_w = true;
                return;
            }
            KeyCode::Down | KeyCode::Char('j') => self.select_next(1),
            KeyCode::Up | KeyCode::Char('k') => self.select_previous(1),
            KeyCode::PageDown => self.select_next(10),
            KeyCode::PageUp => self.select_previous(10),
            KeyCode::Char('d') if control => self.select_next(10),
            KeyCode::Char('u') if control => self.select_previous(10),
            KeyCode::Home => self.select_first(),
            KeyCode::End | KeyCode::Char('G') => self.select_last(),
            KeyCode::Char('g') if self.pending_g => self.select_first(),
            KeyCode::Char('g') => {
                self.pending_g = true;
                return;
            }
            KeyCode::Char(':') => {
                self.mode = Mode::Command;
                self.command.clear();
                self.command_cursor = 0;
                self.history_index = None;
                self.history_draft.clear();
                self.message = None;
            }
            _ => {}
        }

        self.pending_g = false;
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return;
        }
        if let Some(view) = self
            .pane_areas
            .iter()
            .find(|(_, area)| area.contains((mouse.column, mouse.row).into()))
            .map(|(view, _)| *view)
        {
            self.active_tab_mut().active_view = view;
            self.message = Some(format!("View {} focused", view + 1));
        }
    }

    fn move_view_focus(&mut self, direction: FocusDirection) {
        let active = self.active_tab().active_view;
        let Some((_, current)) = self.pane_areas.iter().find(|(view, _)| *view == active) else {
            return;
        };
        let current_center = rect_center(*current);
        let next = self
            .pane_areas
            .iter()
            .filter(|(view, _)| *view != active)
            .filter_map(|(view, area)| {
                let center = rect_center(*area);
                let primary = match direction {
                    FocusDirection::Left if center.0 < current_center.0 => {
                        current_center.0 - center.0
                    }
                    FocusDirection::Right if center.0 > current_center.0 => {
                        center.0 - current_center.0
                    }
                    FocusDirection::Up if center.1 < current_center.1 => {
                        current_center.1 - center.1
                    }
                    FocusDirection::Down if center.1 > current_center.1 => {
                        center.1 - current_center.1
                    }
                    _ => return None,
                };
                let secondary = match direction {
                    FocusDirection::Left | FocusDirection::Right => {
                        current_center.1.abs_diff(center.1)
                    }
                    FocusDirection::Up | FocusDirection::Down => {
                        current_center.0.abs_diff(center.0)
                    }
                };
                Some((*view, u32::from(primary) * 10_000 + u32::from(secondary)))
            })
            .min_by_key(|(_, score)| *score)
            .map(|(view, _)| view);
        if let Some(next) = next {
            self.active_tab_mut().active_view = next;
            self.message = Some(format!("View {} focused", next + 1));
        }
    }

    fn handle_command_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.command.clear();
                self.command_cursor = 0;
                self.history_index = None;
                self.history_draft.clear();
            }
            KeyCode::Enter => self.execute_command(),
            KeyCode::Left => {
                self.command_cursor = self.command_cursor.saturating_sub(1);
            }
            KeyCode::Right => {
                self.command_cursor = (self.command_cursor + 1).min(self.command.chars().count());
            }
            KeyCode::Home => self.command_cursor = 0,
            KeyCode::End => self.command_cursor = self.command.chars().count(),
            KeyCode::Up => self.previous_command(),
            KeyCode::Down => self.next_command(),
            KeyCode::Backspace => self.backspace_command(),
            KeyCode::Delete => self.delete_command_character(),
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                let byte_index = char_to_byte_index(&self.command, self.command_cursor);
                self.command.insert(byte_index, character);
                self.command_cursor += 1;
            }
            _ => {}
        }
    }

    fn execute_command(&mut self) {
        let command = self.command.trim().to_owned();
        if !command.is_empty() {
            self.record_command(command.clone());
        }

        if let Some(count) = parse_delete_command(&command) {
            match count {
                Ok(count) => self.delete_rows(count),
                Err(message) => self.message = Some(message),
            }
            self.finish_command();
            return;
        }

        let (name, argument) = command
            .split_once(char::is_whitespace)
            .map_or((command.as_str(), None), |(name, argument)| {
                (name, Some(argument.trim()))
            });
        match name {
            "q" if argument.is_none() => {
                self.should_quit = true;
                return;
            }
            "w" => {
                self.write_state(argument);
            }
            "wq" => {
                if self.write_state(argument) {
                    self.should_quit = true;
                    return;
                }
            }
            "e" => self.edit_state(argument),
            "refresh" if argument.is_none() => self.refresh(),
            "refresh-rate" => self.set_refresh_rate(argument),
            "filter" => self.set_filter(argument),
            "sort" => self.set_sort(argument),
            "split" => self.split_view(argument),
            "resize" => self.resize_command(argument),
            "tabnew" => self.tab_new(argument),
            "tabsetname" => self.tab_set_name(argument),
            "triage" if argument.is_none() => self.start_triage(),
            "tabnext" | "tabn" if argument.is_none() => self.tab_next(),
            "tabprevious" | "tabp" if argument.is_none() => self.tab_previous(),
            "tabclose" | "tabc" if argument.is_none() => self.tab_close(),
            "" => {}
            _ => self.message = Some(format!("E492: Not an editor command: {command}")),
        }

        self.finish_command();
    }

    fn record_command(&mut self, command: String) {
        if self.command_history.last() != Some(&command) {
            self.command_history.push(command);
        }
        let excess = self
            .command_history
            .len()
            .saturating_sub(MAX_COMMAND_HISTORY);
        if excess > 0 {
            self.command_history.drain(..excess);
        }
        if let Some(path) = self.command_history_path.as_ref()
            && let Err(error) = save_command_history(path, &self.command_history)
        {
            self.message = Some(format!("E886: Could not save command history: {error}"));
        }
    }

    fn finish_command(&mut self) {
        self.command.clear();
        self.command_cursor = 0;
        self.history_index = None;
        self.history_draft.clear();
        self.mode = Mode::Normal;
    }

    fn delete_rows(&mut self, count: usize) {
        let Some(selected) = self.active_view().table_state.selected() else {
            self.message = Some("E749: Empty buffer".to_owned());
            return;
        };

        let deleted_ids = self
            .visible_workflows()
            .into_iter()
            .skip(selected)
            .take(count)
            .map(|workflow| workflow.id)
            .collect::<Vec<_>>();
        let deleted = deleted_ids.len();
        self.active_view_mut()
            .workflow_ids
            .retain(|id| !deleted_ids.contains(id));

        let visible_count = self.visible_workflows().len();
        let next_selection = if visible_count == 0 {
            None
        } else {
            Some(selected.min(visible_count - 1))
        };
        self.active_view_mut().table_state.select(next_selection);
        self.message = Some(if deleted == 1 {
            "1 workflow deleted".to_owned()
        } else {
            format!("{deleted} workflows deleted")
        });
    }

    fn write_state(&mut self, argument: Option<&str>) -> bool {
        let path = match command_path(argument, self.state_path.as_ref()) {
            Ok(path) => path,
            Err(message) => {
                self.message = Some(message);
                return false;
            }
        };

        let (tabs, active_tab) = self.persisted_tabs();
        let state = ViewState::new(self.repository.clone(), &self.workflows, tabs, active_tab);
        match state.save(&path) {
            Ok(()) => {
                self.message = Some(format!(
                    "\"{}\" {} workflows written",
                    path.display(),
                    self.workflows.len()
                ));
                self.state_path = Some(path);
                true
            }
            Err(error) => {
                self.message = Some(format!("E212: {error}"));
                false
            }
        }
    }

    fn persisted_tabs(&self) -> (Vec<ViewTab>, usize) {
        let normal_tabs = self
            .tabs
            .iter()
            .enumerate()
            .filter(|(_, tab)| !tab.is_triage)
            .map(|(index, tab)| {
                let persisted_views = tab
                    .views
                    .iter()
                    .map(|view| ViewPane {
                        workflow_ids: view.workflow_ids.clone(),
                        filter: view.filter.clone(),
                        sort: view.sort.clone(),
                        selected_workflow_id: view
                            .table_state
                            .selected()
                            .and_then(|selected| {
                                self.visible_workflows_for(view).get(selected).copied()
                            })
                            .map(|workflow| workflow.id),
                    })
                    .collect::<Vec<_>>();
                let primary = &persisted_views[0];
                (
                    index,
                    ViewTab {
                        name: tab.name.clone(),
                        workflow_ids: primary.workflow_ids.clone(),
                        filter: primary.filter.clone(),
                        sort: primary.sort.clone(),
                        selected_workflow_id: primary.selected_workflow_id,
                        additional_views: persisted_views.into_iter().skip(1).collect(),
                        layout: tab.layout.clone(),
                        active_view: tab.active_view,
                    },
                )
            })
            .collect::<Vec<_>>();
        if normal_tabs.is_empty() {
            return (
                vec![ViewTab {
                    name: None,
                    workflow_ids: self.workflows.iter().map(|workflow| workflow.id).collect(),
                    filter: None,
                    sort: None,
                    selected_workflow_id: self.workflows.first().map(|workflow| workflow.id),
                    additional_views: Vec::new(),
                    layout: ViewLayout::default(),
                    active_view: 0,
                }],
                0,
            );
        }

        let active_tab = normal_tabs
            .iter()
            .rposition(|(index, _)| *index <= self.active_tab)
            .unwrap_or(0);
        (
            normal_tabs.into_iter().map(|(_, tab)| tab).collect(),
            active_tab,
        )
    }

    fn edit_state(&mut self, argument: Option<&str>) {
        let path = match command_path(argument, self.state_path.as_ref()) {
            Ok(path) => path,
            Err(message) => {
                self.message = Some(message);
                return;
            }
        };

        match ViewState::load(&path) {
            Ok(state) => self.start_loading(PendingLoad {
                repository: state.repository,
                workflow_ids: Some(state.workflow_ids),
                state_path: Some(path),
                kind: LoadKind::Edit,
                tabs: Some(state.tabs),
                active_tab: state.active_tab,
            }),
            Err(error) => self.message = Some(format!("E484: {error}")),
        }
    }

    fn set_refresh_rate(&mut self, argument: Option<&str>) {
        let Some(argument) = argument.filter(|argument| !argument.is_empty()) else {
            self.message = Some("E471: Argument required".to_owned());
            return;
        };
        let Ok(seconds) = argument.parse::<u64>() else {
            self.message = Some(format!("E474: Invalid argument: {argument}"));
            return;
        };
        if seconds == 0 {
            self.message = Some("E474: Refresh rate must be greater than zero".to_owned());
            return;
        }

        self.refresh_interval = Duration::from_secs(seconds);
        self.next_refresh = Instant::now() + self.refresh_interval;
        self.message = Some(format!("Refresh rate set to {seconds} seconds"));
    }

    fn set_filter(&mut self, argument: Option<&str>) {
        let Some(expression) = argument.filter(|argument| !argument.is_empty()) else {
            self.active_view_mut().filter = None;
            self.active_view_mut().filter_expression = None;
            self.reset_visible_selection();
            self.message = Some("Filter cleared".to_owned());
            return;
        };

        match FilterExpression::parse(expression) {
            Ok(filter) => {
                self.active_view_mut().filter = Some(expression.to_owned());
                self.active_view_mut().filter_expression = Some(filter);
                self.reset_visible_selection();
                self.message = Some(format!("Filter: {expression}"));
            }
            Err(error) => self.message = Some(format!("E474: {error}")),
        }
    }

    fn set_sort(&mut self, argument: Option<&str>) {
        let Some(specification) = argument.filter(|argument| !argument.is_empty()) else {
            self.active_view_mut().sort = None;
            self.active_view_mut().sort_spec = None;
            self.reset_visible_selection();
            self.message = Some("Sort cleared".to_owned());
            return;
        };

        match SortSpec::parse(specification) {
            Ok(sort) => {
                self.active_view_mut().sort = Some(specification.to_owned());
                self.active_view_mut().sort_spec = Some(sort);
                self.reset_visible_selection();
                self.message = Some(format!("Sort: {specification}"));
            }
            Err(error) => self.message = Some(format!("E474: {error}")),
        }
    }

    fn reset_visible_selection(&mut self) {
        let has_visible = !self.visible_workflows().is_empty();
        self.active_view_mut()
            .table_state
            .select(has_visible.then_some(0));
    }

    fn split_view(&mut self, argument: Option<&str>) {
        if self.active_tab().is_triage {
            self.message = Some("E474: Triage views cannot be split".to_owned());
            return;
        }
        let direction = match argument {
            Some("horizontal") => SplitDirection::Horizontal,
            Some("vertical") => SplitDirection::Vertical,
            Some(argument) => {
                self.message = Some(format!("E474: Invalid split direction: {argument}"));
                return;
            }
            None => {
                self.message = Some("E471: Argument required".to_owned());
                return;
            }
        };

        let tab = self.active_tab_mut();
        let source_index = tab.active_view;
        let new_index = tab.views.len();
        tab.views.push(tab.views[source_index].clone());
        split_layout_leaf(&mut tab.layout, source_index, new_index, direction);
        tab.active_view = new_index;
        self.message = Some(match direction {
            SplitDirection::Horizontal => "View split horizontally".to_owned(),
            SplitDirection::Vertical => "View split vertically".to_owned(),
        });
    }

    fn resize_command(&mut self, argument: Option<&str>) {
        let Some(argument) = argument.filter(|argument| !argument.is_empty()) else {
            self.message = Some("E471: Argument required".to_owned());
            return;
        };
        if argument == "equal" {
            self.equalize_splits();
            return;
        }

        let mut parts = argument.split_whitespace();
        let axis = match parts.next() {
            Some("height") => ResizeAxis::Height,
            Some("width") => ResizeAxis::Width,
            Some(value) => {
                self.message = Some(format!("E474: Invalid resize dimension: {value}"));
                return;
            }
            None => unreachable!(),
        };
        let Some(amount_text) = parts.next() else {
            self.message = Some("E471: Resize amount required".to_owned());
            return;
        };
        if parts.next().is_some() {
            self.message = Some(format!("E488: Trailing characters: {argument}"));
            return;
        }
        let amount = if let Some(value) = amount_text.strip_prefix('+') {
            value.parse::<i16>().ok().map(ResizeAmount::Delta)
        } else if let Some(value) = amount_text.strip_prefix('-') {
            value
                .parse::<i16>()
                .ok()
                .and_then(|value| value.checked_neg())
                .map(ResizeAmount::Delta)
        } else {
            amount_text.parse::<u16>().ok().map(ResizeAmount::Absolute)
        };
        match amount {
            Some(ResizeAmount::Delta(0) | ResizeAmount::Absolute(0)) | None => {
                self.message = Some(format!("E474: Invalid resize amount: {amount_text}"));
            }
            Some(amount) => self.resize_active_view(axis, amount),
        }
    }

    fn equalize_splits(&mut self) {
        if self.active_tab().is_triage {
            self.message = Some("E474: Triage views cannot be resized".to_owned());
            return;
        }
        equalize_layout(&mut self.active_tab_mut().layout);
        self.message = Some("Split views equalized".to_owned());
    }

    fn resize_active_view(&mut self, axis: ResizeAxis, amount: ResizeAmount) {
        if self.active_tab().is_triage {
            self.message = Some("E474: Triage views cannot be resized".to_owned());
            return;
        }
        let active = self.active_tab().active_view;
        let Some(root_area) = bounding_rect(&self.pane_areas) else {
            self.message = Some("E474: View layout is not available".to_owned());
            return;
        };
        let direction = match axis {
            ResizeAxis::Height => SplitDirection::Horizontal,
            ResizeAxis::Width => SplitDirection::Vertical,
        };
        let Some(target) =
            nearest_resize_target(&self.active_tab().layout, root_area, active, direction)
        else {
            self.message = Some(format!(
                "Active view has no resizable {} split",
                match direction {
                    SplitDirection::Horizontal => "horizontal",
                    SplitDirection::Vertical => "vertical",
                }
            ));
            return;
        };
        let Some((_, active_area)) = self.pane_areas.iter().find(|(index, _)| *index == active)
        else {
            self.message = Some("E474: Active view geometry is not available".to_owned());
            return;
        };
        let current = match axis {
            ResizeAxis::Height => active_area.height,
            ResizeAxis::Width => active_area.width,
        };
        let delta = match amount {
            ResizeAmount::Absolute(target) => i32::from(target) - i32::from(current),
            ResizeAmount::Delta(delta) => i32::from(delta),
        };
        let minimum = match axis {
            ResizeAxis::Height => MIN_VIEW_HEIGHT,
            ResizeAxis::Width => MIN_VIEW_WIDTH,
        };
        match resize_layout_split(
            &mut self.active_tab_mut().layout,
            &target.path,
            target.area,
            target.active_in_first,
            delta,
            minimum,
        ) {
            Ok(()) => {
                self.message = Some(format!(
                    "Active view {} adjusted",
                    match axis {
                        ResizeAxis::Height => "height",
                        ResizeAxis::Width => "width",
                    }
                ));
            }
            Err(message) => self.message = Some(message),
        }
    }

    fn tab_new(&mut self, name: Option<&str>) {
        let mut tab = self.active_tab().clone();
        tab.name = name.filter(|name| !name.is_empty()).map(ToOwned::to_owned);
        self.tabs.insert(self.active_tab + 1, tab);
        self.active_tab += 1;
        self.message = Some(format!("{} opened", self.active_tab_label()));
    }

    fn start_triage(&mut self) {
        if self.triage_receiver.is_some() {
            self.message = Some("Triage already in progress".to_owned());
            return;
        }
        let workflows = self
            .visible_workflows()
            .into_iter()
            .filter(|workflow| workflow.run_status == RunStatus::Failure)
            .cloned()
            .collect::<Vec<_>>();
        if workflows.is_empty() {
            self.message = Some("No failing workflows in the active view".to_owned());
            return;
        }
        let source = Arc::clone(&self.workflow_source);
        let repository = self.repository.clone();
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let _ = sender.send(source.triage_workflows(&repository, &workflows));
        });
        self.triage_receiver = Some(receiver);
        self.triage_target = Some(TriageTarget::NewTab {
            insert_after: self.active_tab,
        });
        self.message = None;
    }

    fn refresh_triage(&mut self) {
        if self.triage_receiver.is_some() {
            self.message = Some("Triage already in progress".to_owned());
            return;
        }
        let workflows = self
            .visible_workflows()
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        if workflows.is_empty() {
            self.message = Some("No workflows in the triage view".to_owned());
            return;
        }
        let source = Arc::clone(&self.workflow_source);
        let repository = self.repository.clone();
        let target = self.active_tab;
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let _ = sender.send(source.triage_workflows(&repository, &workflows));
        });
        self.triage_receiver = Some(receiver);
        self.triage_target = Some(TriageTarget::RefreshTab { index: target });
        self.message = None;
    }

    fn tab_set_name(&mut self, name: Option<&str>) {
        let Some(name) = name.filter(|name| !name.is_empty()) else {
            self.message = Some("E471: Argument required".to_owned());
            return;
        };

        self.active_tab_mut().name = Some(name.to_owned());
        self.message = Some(format!("Tab renamed to {name}"));
    }

    fn tab_next(&mut self) {
        self.active_tab = (self.active_tab + 1) % self.tabs.len();
        self.message = Some(format!("Tab {}", self.active_tab + 1));
    }

    fn tab_previous(&mut self) {
        self.active_tab = self
            .active_tab
            .checked_sub(1)
            .unwrap_or(self.tabs.len() - 1);
        self.message = Some(format!("Tab {}", self.active_tab + 1));
    }

    fn tab_close(&mut self) {
        if self.tabs.len() == 1 {
            self.tabs[0] = Tab::new(None, Vec::new(), None, None);
            self.message = Some("Last tab reset".to_owned());
            return;
        }

        self.tabs.remove(self.active_tab);
        self.active_tab = self.active_tab.min(self.tabs.len() - 1);
        self.message = Some(format!("Tab {} closed", self.active_tab + 1));
    }

    fn active_tab_label(&self) -> String {
        self.active_tab()
            .name
            .clone()
            .unwrap_or_else(|| format!("Tab {}", self.active_tab + 1))
    }

    fn backspace_command(&mut self) {
        if self.command_cursor == 0 {
            return;
        }

        let start = char_to_byte_index(&self.command, self.command_cursor - 1);
        let end = char_to_byte_index(&self.command, self.command_cursor);
        self.command.replace_range(start..end, "");
        self.command_cursor -= 1;
    }

    fn delete_command_character(&mut self) {
        if self.command_cursor == self.command.chars().count() {
            return;
        }

        let start = char_to_byte_index(&self.command, self.command_cursor);
        let end = char_to_byte_index(&self.command, self.command_cursor + 1);
        self.command.replace_range(start..end, "");
    }

    fn previous_command(&mut self) {
        if self.command_history.is_empty() {
            return;
        }

        let index = match self.history_index {
            Some(index) => index.saturating_sub(1),
            None => {
                self.history_draft.clone_from(&self.command);
                self.command_history.len() - 1
            }
        };
        self.load_history(index);
    }

    fn next_command(&mut self) {
        let Some(index) = self.history_index else {
            return;
        };

        if index + 1 < self.command_history.len() {
            self.load_history(index + 1);
        } else {
            self.command.clone_from(&self.history_draft);
            self.command_cursor = self.command.chars().count();
            self.history_index = None;
        }
    }

    fn load_history(&mut self, index: usize) {
        self.command.clone_from(&self.command_history[index]);
        self.command_cursor = self.command.chars().count();
        self.history_index = Some(index);
    }

    fn select_next(&mut self, amount: usize) {
        let visible_count = self.visible_workflows().len();
        if visible_count == 0 {
            return;
        }

        let current = self.active_view().table_state.selected().unwrap_or(0);
        self.active_view_mut()
            .table_state
            .select(Some((current + amount).min(visible_count - 1)));
    }

    fn select_previous(&mut self, amount: usize) {
        if self.visible_workflows().is_empty() {
            return;
        }

        let current = self.active_view().table_state.selected().unwrap_or(0);
        self.active_view_mut()
            .table_state
            .select(Some(current.saturating_sub(amount)));
    }

    fn select_first(&mut self) {
        if !self.visible_workflows().is_empty() {
            self.active_view_mut().table_state.select(Some(0));
        }
    }

    fn select_last(&mut self) {
        let visible_count = self.visible_workflows().len();
        if visible_count > 0 {
            self.active_view_mut()
                .table_state
                .select(Some(visible_count - 1));
        }
    }

    fn render(&mut self, frame: &mut Frame) {
        let (tabs_area, table_area, help_area) = if self.tabs.is_empty() {
            let [table_area, help_area] =
                Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(frame.area());
            (None, table_area, help_area)
        } else {
            let [tabs_area, table_area, help_area] = Layout::vertical([
                Constraint::Length(1),
                Constraint::Min(3),
                Constraint::Length(1),
            ])
            .areas(frame.area());
            (Some(tabs_area), table_area, help_area)
        };

        if let Some(tabs_area) = tabs_area {
            let tab_titles = (0..self.tabs.len())
                .map(|index| {
                    Line::from(format!(
                        " {} ",
                        self.tabs[index]
                            .name
                            .as_deref()
                            .map_or_else(|| format!("Tab {}", index + 1), ToOwned::to_owned)
                    ))
                })
                .collect::<Vec<_>>();
            let tabs = Tabs::new(tab_titles)
                .select(self.active_tab)
                .style(Style::default().fg(Color::White).bg(Color::DarkGray))
                .highlight_style(
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
                .divider(" ");
            frame.render_widget(tabs, tabs_area);
        }

        self.pane_areas.clear();
        if self.active_tab().is_triage {
            let visible_workflows = self
                .visible_workflows()
                .into_iter()
                .cloned()
                .collect::<Vec<_>>();
            self.pane_areas.push((0, table_area));
            if visible_workflows.is_empty() {
                let empty_message = if self.triage_receiver.is_some() {
                    "Triaging failing workflows..."
                } else {
                    "No failing scheduled workflows were found."
                };
                let empty = Paragraph::new(empty_message).centered().block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(format!(" {} ", self.repository)),
                );
                frame.render_widget(empty, table_area);
            } else {
                self.render_triage_blocks(frame, table_area, &visible_workflows);
            }
        } else {
            let pane_areas = layout_areas(&self.active_tab().layout, table_area);
            self.pane_areas.clone_from(&pane_areas);
            let active_tab = self.active_tab;
            let loading_status_ids = self.loading_status_ids.clone();
            for (view_index, area) in pane_areas {
                let workflows = self
                    .visible_workflows_for(&self.tabs[active_tab].views[view_index])
                    .into_iter()
                    .cloned()
                    .collect::<Vec<_>>();
                let is_filtered = self.tabs[active_tab].views[view_index].filter.is_some();
                let active = view_index == self.tabs[active_tab].active_view;
                let repository = self.repository.clone();
                let loading = self.is_loading();
                let flash = flash_visible(self.animation_started.elapsed());
                render_list_view(
                    frame,
                    area,
                    &repository,
                    &workflows,
                    &mut self.tabs[active_tab].views[view_index].table_state,
                    active,
                    loading,
                    is_filtered,
                    flash,
                    &loading_status_ids,
                );
            }
        }

        match self.mode {
            Mode::Normal => {
                let (title, text) = self.normal_status();
                render_status_bar(frame, help_area, &text, &self.refresh_rate_label(), title);
            }
            Mode::Command => {
                const COMMAND_PREFIX: &str = ":";
                let left_area = render_status_bar(
                    frame,
                    help_area,
                    &format!("{COMMAND_PREFIX}{}", self.command),
                    &self.refresh_rate_label(),
                    " COMMAND ",
                );

                let cursor_x = if left_area.width == 0 {
                    help_area.x
                } else {
                    left_area
                        .x
                        .saturating_add(COMMAND_PREFIX.chars().count() as u16)
                        .saturating_add(self.command_cursor as u16)
                        .min(left_area.right().saturating_sub(1))
                };
                frame.set_cursor_position((cursor_x, help_area.y));
            }
        }
    }

    fn render_triage_blocks(&self, frame: &mut Frame, area: Rect, visible_workflows: &[Workflow]) {
        let selected = self.active_view().table_state.selected().unwrap_or(0);
        let mut y = area.y;
        let summary = triage_correlation_summary(visible_workflows, &self.triage);
        let summary_height = triage_block_height(&summary, area.width)
            .min(area.height)
            .max(1);
        let summary_area = Rect::new(area.x, y, area.width, summary_height);
        let summary_panel = Paragraph::new(summary).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Tests across multiple workflows "),
        );
        frame.render_widget(summary_panel, summary_area);
        y = y.saturating_add(summary_height);

        for (index, workflow) in visible_workflows.iter().enumerate().skip(selected) {
            if y >= area.bottom() {
                break;
            }
            let triage = self.triage.get(&workflow.id);
            let text = triage_text(triage);
            let available_height = area.bottom() - y;
            let height = triage_block_height(&text, area.width)
                .min(available_height)
                .max(1);
            let block_area = Rect::new(area.x, y, area.width, height);
            let selected_style = if index == selected {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let panel = Paragraph::new(text).wrap(Wrap { trim: false }).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(selected_style)
                    .title(format!(" {} ", workflow.name)),
            );
            frame.render_widget(panel, block_area);
            y = y.saturating_add(height);
        }
    }
}

fn split_layout_leaf(
    layout: &mut ViewLayout,
    target: usize,
    new_index: usize,
    direction: SplitDirection,
) -> bool {
    match layout {
        ViewLayout::Pane { index } if *index == target => {
            *layout = ViewLayout::Split {
                direction,
                ratio: EQUAL_SPLIT_RATIO,
                first: Box::new(ViewLayout::Pane { index: target }),
                second: Box::new(ViewLayout::Pane { index: new_index }),
            };
            true
        }
        ViewLayout::Pane { .. } => false,
        ViewLayout::Split { first, second, .. } => {
            split_layout_leaf(first, target, new_index, direction)
                || split_layout_leaf(second, target, new_index, direction)
        }
    }
}

fn layout_areas(layout: &ViewLayout, area: Rect) -> Vec<(usize, Rect)> {
    fn collect(layout: &ViewLayout, area: Rect, areas: &mut Vec<(usize, Rect)>) {
        match layout {
            ViewLayout::Pane { index } => areas.push((*index, area)),
            ViewLayout::Split {
                direction,
                ratio,
                first,
                second,
            } => {
                let [first_area, second_area] = match direction {
                    SplitDirection::Horizontal => Layout::vertical([
                        Constraint::Ratio(*ratio as u32, SPLIT_RATIO_SCALE as u32),
                        Constraint::Ratio(
                            (SPLIT_RATIO_SCALE - *ratio) as u32,
                            SPLIT_RATIO_SCALE as u32,
                        ),
                    ])
                    .areas(area),
                    SplitDirection::Vertical => Layout::horizontal([
                        Constraint::Ratio(*ratio as u32, SPLIT_RATIO_SCALE as u32),
                        Constraint::Ratio(
                            (SPLIT_RATIO_SCALE - *ratio) as u32,
                            SPLIT_RATIO_SCALE as u32,
                        ),
                    ])
                    .areas(area),
                };
                collect(first, first_area, areas);
                collect(second, second_area, areas);
            }
        }
    }

    let mut areas = Vec::new();
    collect(layout, area, &mut areas);
    areas
}

struct ResizeTarget {
    path: Vec<bool>,
    area: Rect,
    active_in_first: bool,
}

fn layout_contains(layout: &ViewLayout, target: usize) -> bool {
    match layout {
        ViewLayout::Pane { index } => *index == target,
        ViewLayout::Split { first, second, .. } => {
            layout_contains(first, target) || layout_contains(second, target)
        }
    }
}

fn split_child_areas(direction: SplitDirection, ratio: u16, area: Rect) -> (Rect, Rect) {
    let constraints = [
        Constraint::Ratio(ratio as u32, SPLIT_RATIO_SCALE as u32),
        Constraint::Ratio((SPLIT_RATIO_SCALE - ratio) as u32, SPLIT_RATIO_SCALE as u32),
    ];
    match direction {
        SplitDirection::Horizontal => {
            let [first, second] = Layout::vertical(constraints).areas(area);
            (first, second)
        }
        SplitDirection::Vertical => {
            let [first, second] = Layout::horizontal(constraints).areas(area);
            (first, second)
        }
    }
}

fn nearest_resize_target(
    layout: &ViewLayout,
    area: Rect,
    active: usize,
    direction: SplitDirection,
) -> Option<ResizeTarget> {
    fn find(
        layout: &ViewLayout,
        area: Rect,
        active: usize,
        direction: SplitDirection,
        path: &mut Vec<bool>,
    ) -> Option<ResizeTarget> {
        let ViewLayout::Split {
            direction: split_direction,
            ratio,
            first,
            second,
        } = layout
        else {
            return None;
        };
        let active_in_first = layout_contains(first, active);
        let active_in_second = layout_contains(second, active);
        if !active_in_first && !active_in_second {
            return None;
        }
        let (first_area, second_area) = split_child_areas(*split_direction, *ratio, area);
        path.push(!active_in_first);
        let child = if active_in_first { first } else { second };
        let child_area = if active_in_first {
            first_area
        } else {
            second_area
        };
        if let Some(target) = find(child, child_area, active, direction, path) {
            path.pop();
            return Some(target);
        }
        path.pop();

        (*split_direction == direction).then(|| ResizeTarget {
            path: path.clone(),
            area,
            active_in_first,
        })
    }

    find(layout, area, active, direction, &mut Vec::new())
}

fn resize_layout_split(
    layout: &mut ViewLayout,
    path: &[bool],
    area: Rect,
    active_in_first: bool,
    delta: i32,
    minimum: u16,
) -> Result<(), String> {
    let mut node = layout;
    for second in path {
        let ViewLayout::Split {
            first,
            second: second_layout,
            ..
        } = node
        else {
            return Err("E474: Saved split layout is invalid".to_owned());
        };
        node = if *second { second_layout } else { first };
    }
    let ViewLayout::Split {
        direction, ratio, ..
    } = node
    else {
        return Err("E474: Saved split layout is invalid".to_owned());
    };
    let total = match direction {
        SplitDirection::Horizontal => area.height,
        SplitDirection::Vertical => area.width,
    };
    if total < minimum.saturating_mul(2) {
        return Err(format!(
            "Split is too small to keep both views at least {minimum} cells"
        ));
    }
    let (first_area, _) = split_child_areas(*direction, *ratio, area);
    let first_size = match direction {
        SplitDirection::Horizontal => first_area.height,
        SplitDirection::Vertical => first_area.width,
    };
    let first_delta = if active_in_first { delta } else { -delta };
    let first_size = (i32::from(first_size) + first_delta)
        .clamp(i32::from(minimum), i32::from(total - minimum)) as u16;
    *ratio = ((u32::from(first_size) * u32::from(SPLIT_RATIO_SCALE) + u32::from(total) / 2)
        / u32::from(total)) as u16;
    *ratio = (*ratio).clamp(1, SPLIT_RATIO_SCALE - 1);
    Ok(())
}

fn equalize_layout(layout: &mut ViewLayout) -> usize {
    match layout {
        ViewLayout::Pane { .. } => 1,
        ViewLayout::Split {
            ratio,
            first,
            second,
            ..
        } => {
            let first_count = equalize_layout(first);
            let second_count = equalize_layout(second);
            let total = first_count + second_count;
            *ratio =
                u16::try_from((first_count * usize::from(SPLIT_RATIO_SCALE) + total / 2) / total)
                    .unwrap_or(EQUAL_SPLIT_RATIO)
                    .clamp(1, SPLIT_RATIO_SCALE - 1);
            total
        }
    }
}

fn bounding_rect(areas: &[(usize, Rect)]) -> Option<Rect> {
    let first = areas.first()?.1;
    let left = areas.iter().map(|(_, area)| area.x).min()?;
    let top = areas.iter().map(|(_, area)| area.y).min()?;
    let right = areas
        .iter()
        .map(|(_, area)| area.right())
        .max()
        .unwrap_or(first.right());
    let bottom = areas
        .iter()
        .map(|(_, area)| area.bottom())
        .max()
        .unwrap_or(first.bottom());
    Some(Rect::new(
        left,
        top,
        right.saturating_sub(left),
        bottom.saturating_sub(top),
    ))
}

fn rect_center(area: Rect) -> (u16, u16) {
    (
        area.x.saturating_add(area.width / 2),
        area.y.saturating_add(area.height / 2),
    )
}

#[allow(clippy::too_many_arguments)]
fn render_list_view(
    frame: &mut Frame,
    area: Rect,
    repository: &Repository,
    workflows: &[Workflow],
    table_state: &mut TableState,
    active: bool,
    loading: bool,
    is_filtered: bool,
    flash_visible: bool,
    loading_status_ids: &BTreeSet<u64>,
) {
    let border_style = if active {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(format!(
            " {repository} - {} active workflows ",
            workflows.len()
        ));

    if workflows.is_empty() {
        let message = if loading {
            "Loading GitHub Actions workflows..."
        } else if is_filtered {
            "No workflows match this view's filter."
        } else {
            "This view has no active GitHub Actions workflows."
        };
        frame.render_widget(Paragraph::new(message).centered().block(block), area);
        return;
    }

    let header = Row::new(["Status", "Name", "24 Hours", "7 Days", "14 Days"])
        .style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .bottom_margin(1);
    let rows = workflows.iter().map(|workflow| {
        if loading_status_ids.contains(&workflow.id) {
            return Row::new([
                Cell::from("..."),
                Cell::from(workflow.name.as_str()),
                Cell::from("loading..."),
                Cell::from("loading..."),
                Cell::from("loading..."),
            ]);
        }
        Row::new([
            Cell::from(status_indicator(workflow, flash_visible)),
            Cell::from(workflow.name.as_str()),
            metrics_cell(workflow.run_metrics.last_24_hours),
            metrics_cell(workflow.run_metrics.last_7_days),
            metrics_cell(workflow.run_metrics.last_14_days),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(8),
            Constraint::Min(20),
            Constraint::Length(19),
            Constraint::Length(19),
            Constraint::Length(19),
        ],
    )
    .header(header)
    .row_highlight_style(
        Style::default()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    )
    .highlight_symbol(">> ")
    .block(block);
    frame.render_stateful_widget(table, area, table_state);
}

fn render_status_bar(
    frame: &mut Frame,
    area: Rect,
    text: &str,
    refresh_rate: &str,
    status: &str,
) -> Rect {
    let bar_style = Style::default().fg(Color::White).bg(Color::DarkGray);
    frame.render_widget(Block::default().style(bar_style), area);

    let suffix_width = (refresh_rate.chars().count() + status.chars().count() + 2)
        .min(usize::from(area.width)) as u16;
    let [left_area, right_area] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(suffix_width)]).areas(area);

    frame.render_widget(Paragraph::new(text.to_owned()).style(bar_style), left_area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw(format!("{refresh_rate}  ")),
            Span::styled(
                status.to_owned(),
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        ]))
        .style(bar_style),
        right_area,
    );

    left_area
}

#[derive(Clone, Copy)]
enum TestOutcome {
    Failed,
    UnexpectedlyPassed,
}

fn triage_correlation_summary<'a>(
    workflows: &'a [Workflow],
    triage: &'a HashMap<u64, WorkflowTriage>,
) -> Text<'a> {
    let names = workflows
        .iter()
        .map(|workflow| (workflow.id, workflow.name.as_str()))
        .collect::<HashMap<_, _>>();
    let mut failed = BTreeMap::<&str, BTreeSet<&str>>::new();
    let mut unexpectedly_passed = BTreeMap::<&str, BTreeSet<&str>>::new();
    for result in triage.values() {
        let Some(workflow_name) = names.get(&result.workflow_id).copied() else {
            continue;
        };
        let mut workflow_failed = result
            .failed_tests
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        let mut workflow_unexpectedly_passed = result
            .unexpectedly_passed_tests
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        for (outcome, test_name) in lit_test_names(&result.lit_summary) {
            match outcome {
                TestOutcome::Failed => {
                    workflow_failed.insert(test_name);
                }
                TestOutcome::UnexpectedlyPassed => {
                    workflow_unexpectedly_passed.insert(test_name);
                }
            }
        }
        for test_name in workflow_failed {
            failed.entry(test_name).or_default().insert(workflow_name);
        }
        for test_name in workflow_unexpectedly_passed {
            unexpectedly_passed
                .entry(test_name)
                .or_default()
                .insert(workflow_name);
        }
    }

    let failed = failed
        .into_iter()
        .filter(|(_, workflows)| workflows.len() > 1)
        .collect::<Vec<_>>();
    let unexpectedly_passed = unexpectedly_passed
        .into_iter()
        .filter(|(_, workflows)| workflows.len() > 1)
        .collect::<Vec<_>>();
    if failed.is_empty() && unexpectedly_passed.is_empty() {
        return Text::from("No tests failed or unexpectedly passed in multiple workflows.");
    }

    let mut lines = Vec::new();
    append_correlated_tests(&mut lines, "Failed in multiple workflows:", &failed);
    if !failed.is_empty() && !unexpectedly_passed.is_empty() {
        lines.push(Line::default());
    }
    append_correlated_tests(
        &mut lines,
        "Unexpectedly passed in multiple workflows:",
        &unexpectedly_passed,
    );
    Text::from(lines)
}

fn append_correlated_tests<'a>(
    lines: &mut Vec<Line<'a>>,
    heading: &'a str,
    tests: &[(&'a str, BTreeSet<&'a str>)],
) {
    if tests.is_empty() {
        return;
    }
    lines.push(Line::from(heading).bold());
    for (test, workflows) in tests {
        lines.push(Line::from(format!("  {test}")));
        lines.extend(
            workflows
                .iter()
                .map(|workflow| Line::from(format!("    - {workflow}"))),
        );
    }
}

fn lit_test_names(summary: &str) -> Vec<(TestOutcome, &str)> {
    let mut outcome = None;
    let mut tests = Vec::new();
    for line in summary.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("Failed Tests (") {
            outcome = Some(TestOutcome::Failed);
        } else if trimmed.starts_with("Unexpectedly Passed Tests (") {
            outcome = Some(TestOutcome::UnexpectedlyPassed);
        } else if trimmed.is_empty() || trimmed.starts_with("**") {
            outcome = None;
        } else if let (Some(outcome), Some((_, test))) = (outcome, trimmed.split_once("::")) {
            tests.push((outcome, test.trim()));
        }
    }
    tests
}

fn triage_text(triage: Option<&WorkflowTriage>) -> Text<'_> {
    let Some(triage) = triage else {
        return Text::from("(triage details unavailable)");
    };
    let mut lines = vec![
        Line::from(format!("Failed jobs: {}", triage.failed_jobs)),
        Line::from(format!("Failed steps: {}", triage.failed_steps)),
    ];
    if !triage.failed_tests.is_empty() {
        lines.push(Line::default());
        lines.push(Line::from("Failed tests:").bold());
        lines.extend(
            triage
                .failed_tests
                .iter()
                .map(|test| Line::from(format!("  - {test}"))),
        );
    }
    if !triage.unexpectedly_passed_tests.is_empty() {
        lines.push(Line::default());
        lines.push(Line::from("Unexpectedly passed tests:").bold());
        lines.extend(
            triage
                .unexpectedly_passed_tests
                .iter()
                .map(|test| Line::from(format!("  - {test}"))),
        );
    }
    if !triage.lit_summary.is_empty() {
        lines.push(Line::default());
        lines.push(Line::from("lit summary:").bold());
        lines.extend(triage.lit_summary.lines().map(Line::from));
    }
    Text::from(lines)
}

fn triage_block_height(text: &Text<'_>, width: u16) -> u16 {
    let content_width = usize::from(width.saturating_sub(2).max(1));
    let wrapped_lines = text
        .lines
        .iter()
        .map(|line| {
            let width = line.width();
            width.max(1).div_ceil(content_width)
        })
        .sum::<usize>();
    u16::try_from(wrapped_lines)
        .unwrap_or(u16::MAX)
        .saturating_add(2)
}

fn command_path(argument: Option<&str>, remembered: Option<&PathBuf>) -> Result<PathBuf, String> {
    match argument.filter(|argument| !argument.is_empty()) {
        Some(argument) => Ok(PathBuf::from(argument)),
        None => remembered
            .cloned()
            .ok_or_else(|| "E32: No file name".to_owned()),
    }
}

fn parse_delete_command(command: &str) -> Option<Result<usize, String>> {
    let count = command.strip_prefix('d')?;
    if count.is_empty() {
        return Some(Ok(1));
    }
    if !count.chars().all(|character| character.is_ascii_digit()) {
        return None;
    }

    Some(
        count
            .parse::<usize>()
            .map_err(|_| format!("E488: Trailing characters: {count}"))
            .and_then(|count| {
                (count > 0)
                    .then_some(count)
                    .ok_or_else(|| "E16: Invalid range".to_owned())
            }),
    )
}

fn status_indicator(workflow: &Workflow, flash_visible: bool) -> &'static str {
    if workflow.is_in_progress && !flash_visible {
        return "";
    }

    match workflow.run_status {
        RunStatus::Success => "🟢",
        RunStatus::Failure => "🔴",
        RunStatus::Other => "⚪",
    }
}

fn metrics_cell(counts: RunCounts) -> Cell<'static> {
    let percentage = counts.pass_percentage();
    Cell::from(format!(
        "{}/{}/{} {:.1}%",
        counts.passed, counts.failed, counts.total, percentage
    ))
    .style(Style::default().fg(metrics_color(percentage)))
}

fn metrics_color(percentage: f64) -> Color {
    if percentage < 70.0 {
        Color::Red
    } else if percentage < 90.0 {
        Color::Yellow
    } else {
        Color::Green
    }
}

fn flash_visible(elapsed: Duration) -> bool {
    (elapsed.as_millis() / STATUS_FLASH_INTERVAL.as_millis()).is_multiple_of(2)
}

fn char_to_byte_index(value: &str, character_index: usize) -> usize {
    value
        .char_indices()
        .nth(character_index)
        .map_or(value.len(), |(index, _)| index)
}

fn command_history_path() -> Option<PathBuf> {
    if let Some(path) = env::var_os("GH_ACTUI_HISTORY").filter(|path| !path.is_empty()) {
        return Some(PathBuf::from(path));
    }
    if cfg!(windows) {
        return env::var_os("LOCALAPPDATA")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .map(|path| path.join("gh-actui").join("history"));
    }
    env::var_os("XDG_STATE_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .map(|path| path.join("gh-actui").join("history"))
        .or_else(|| {
            env::var_os("HOME")
                .filter(|path| !path.is_empty())
                .map(PathBuf::from)
                .map(|path| {
                    path.join(".local")
                        .join("state")
                        .join("gh-actui")
                        .join("history")
                })
        })
}

fn load_command_history(path: &Path) -> io::Result<Vec<String>> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut history = contents
        .lines()
        .map(str::trim)
        .filter(|command| !command.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    if history.len() > MAX_COMMAND_HISTORY {
        history.drain(..history.len() - MAX_COMMAND_HISTORY);
    }
    Ok(history)
}

fn save_command_history(path: &Path, history: &[String]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut contents = history.join("\n");
    if !contents.is_empty() {
        contents.push('\n');
    }
    fs::write(path, contents)
}

pub fn run(
    repository: Repository,
    workflow_ids: Option<Vec<u64>>,
    tabs: Option<Vec<ViewTab>>,
    active_tab: usize,
    state_path: Option<PathBuf>,
    workflow_source: Arc<dyn WorkflowSource>,
) -> io::Result<()> {
    install_panic_hook();
    let mut terminal = ratatui::init();
    if let Err(error) = execute!(io::stdout(), EnableMouseCapture) {
        ratatui::restore();
        return Err(error);
    }
    let mut app = App::new_loading(
        repository,
        workflow_ids,
        state_path,
        workflow_source,
        tabs,
        active_tab,
    );
    if let Some(path) = command_history_path() {
        match load_command_history(&path) {
            Ok(history) => app.command_history = history,
            Err(error) => {
                app.message = Some(format!("E886: Could not load command history: {error}"));
            }
        }
        app.command_history_path = Some(path);
    } else {
        app.message = Some("E886: Could not determine command history path".to_owned());
    }
    let result = app.run(&mut terminal);
    let _ = execute!(io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

fn install_panic_hook() {
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        restore_terminal();
        original_hook(panic_info);
    }));
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestWorkflowSource;

    impl WorkflowSource for TestWorkflowSource {
        fn list_workflows(
            &self,
            _repository: &Repository,
        ) -> Result<Vec<Workflow>, crate::github::Error> {
            Ok((0..100)
                .map(|id| {
                    let mut workflow = workflow(id);
                    workflow.name = format!("Refreshed Workflow {id}");
                    workflow.run_status = RunStatus::Success;
                    workflow
                })
                .collect())
        }

        fn triage_workflows(
            &self,
            _repository: &Repository,
            workflows: &[Workflow],
        ) -> Result<Vec<crate::github::WorkflowTriage>, crate::github::Error> {
            Ok(workflows
                .iter()
                .map(|workflow| crate::github::WorkflowTriage {
                    workflow_id: workflow.id,
                    failed_jobs: format!("Job {}", workflow.id),
                    failed_steps: "Run HLSL Tests".to_owned(),
                    failed_tests: vec!["example.test".to_owned()],
                    unexpectedly_passed_tests: Vec::new(),
                    lit_summary: "Failed Tests (1): example.test".to_owned(),
                })
                .collect())
        }
    }

    struct ProgressiveWorkflowSource;

    impl WorkflowSource for ProgressiveWorkflowSource {
        fn list_workflows(
            &self,
            _repository: &Repository,
        ) -> Result<Vec<Workflow>, crate::github::Error> {
            Ok(vec![workflow(1), workflow(2)])
        }

        fn load_workflow_status(
            &self,
            _repository: &Repository,
            workflow: &Workflow,
        ) -> Result<Workflow, crate::github::Error> {
            let mut workflow = workflow.clone();
            workflow.run_status = RunStatus::Success;
            Ok(workflow)
        }

        fn triage_workflows(
            &self,
            _repository: &Repository,
            _workflows: &[Workflow],
        ) -> Result<Vec<WorkflowTriage>, crate::github::Error> {
            Ok(Vec::new())
        }
    }

    #[derive(Default)]
    struct RecordingWorkflowSource {
        discovered_ids: Vec<u64>,
        loaded_ids: std::sync::Mutex<Vec<u64>>,
    }

    impl WorkflowSource for RecordingWorkflowSource {
        fn list_workflows(
            &self,
            _repository: &Repository,
        ) -> Result<Vec<Workflow>, crate::github::Error> {
            Ok(self.discovered_ids.iter().copied().map(workflow).collect())
        }

        fn load_workflow_status(
            &self,
            _repository: &Repository,
            workflow: &Workflow,
        ) -> Result<Workflow, crate::github::Error> {
            self.loaded_ids.lock().unwrap().push(workflow.id);
            let mut workflow = workflow.clone();
            workflow.run_status = RunStatus::Success;
            Ok(workflow)
        }

        fn triage_workflows(
            &self,
            _repository: &Repository,
            _workflows: &[Workflow],
        ) -> Result<Vec<WorkflowTriage>, crate::github::Error> {
            Ok(Vec::new())
        }
    }

    fn workflow(id: u64) -> Workflow {
        Workflow {
            id,
            name: format!("Workflow {id}"),
            path: format!(".github/workflows/{id}.yml"),
            state: "active".to_owned(),
            run_status: RunStatus::Other,
            is_in_progress: false,
            run_metrics: crate::github::RunMetrics::default(),
        }
    }

    fn app(workflow_count: u64) -> App {
        let repository = "owner/repository".parse().unwrap();
        let workflows = (0..workflow_count).map(workflow).collect();
        App::new(
            repository,
            workflows,
            None,
            Arc::new(TestWorkflowSource),
            None,
        )
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn enter_command(app: &mut App, command: &str) {
        app.handle_key(key(KeyCode::Char(':')));
        for character in command.chars() {
            app.handle_key(key(KeyCode::Char(character)));
        }
    }

    fn complete_loading(app: &mut App) {
        while app.is_loading() {
            let event = app
                .load_receiver
                .as_ref()
                .unwrap()
                .recv_timeout(Duration::from_secs(1))
                .unwrap();
            app.handle_load_event(event);
        }
    }

    fn complete_triage(app: &mut App) {
        let result = app
            .triage_receiver
            .as_ref()
            .unwrap()
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .map_err(|error| error.to_string());
        app.triage_receiver = None;
        let target = app.triage_target.take();
        app.finish_triage(result, target);
    }

    #[test]
    fn split_command_duplicates_active_view_with_requested_layout() {
        let mut app = app(3);
        enter_command(&mut app, "split vertical");
        app.handle_key(key(KeyCode::Enter));

        assert_eq!(app.active_tab().views.len(), 2);
        assert_eq!(app.active_tab().active_view, 1);
        assert_eq!(app.active_view().workflow_ids, vec![0, 1, 2]);
        assert_eq!(
            app.active_tab().layout,
            ViewLayout::Split {
                direction: SplitDirection::Vertical,
                ratio: EQUAL_SPLIT_RATIO,
                first: Box::new(ViewLayout::Pane { index: 0 }),
                second: Box::new(ViewLayout::Pane { index: 1 }),
            }
        );
    }

    #[test]
    fn filters_and_sorts_apply_only_to_active_split() {
        let mut app = app(3);
        enter_command(&mut app, "split horizontal");
        app.handle_key(key(KeyCode::Enter));
        enter_command(&mut app, "filter name:\"Workflow 1\"");
        app.handle_key(key(KeyCode::Enter));

        assert!(app.active_tab().views[0].filter.is_none());
        assert_eq!(
            app.active_tab().views[1].filter.as_deref(),
            Some("name:\"Workflow 1\"")
        );
        assert_eq!(app.visible_workflows().len(), 1);

        app.pane_areas = layout_areas(&app.active_tab().layout, Rect::new(0, 0, 80, 20));
        app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        app.handle_key(key(KeyCode::Char('k')));
        assert_eq!(app.active_tab().active_view, 0);
        assert_eq!(app.visible_workflows().len(), 3);
    }

    #[test]
    fn mouse_click_focuses_split_view() {
        let mut app = app(2);
        app.split_view(Some("vertical"));
        app.pane_areas = layout_areas(&app.active_tab().layout, Rect::new(0, 0, 100, 20));

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 10,
            row: 5,
            modifiers: KeyModifiers::NONE,
        });

        assert_eq!(app.active_tab().active_view, 0);
    }

    #[test]
    fn persisted_tabs_include_split_layout_and_view_state() {
        let mut app = app(3);
        app.split_view(Some("vertical"));
        app.active_view_mut().filter = Some("status:failure".to_owned());
        app.active_view_mut().filter_expression =
            Some(FilterExpression::parse("status:failure").unwrap());
        app.active_view_mut().sort = Some("name:desc".to_owned());
        app.active_view_mut().sort_spec = Some(SortSpec::parse("name:desc").unwrap());

        let (tabs, active_tab) = app.persisted_tabs();

        assert_eq!(active_tab, 0);
        assert_eq!(tabs[0].additional_views.len(), 1);
        assert_eq!(
            tabs[0].additional_views[0].filter.as_deref(),
            Some("status:failure")
        );
        assert_eq!(
            tabs[0].additional_views[0].sort.as_deref(),
            Some("name:desc")
        );
        assert_eq!(tabs[0].active_view, 1);
        assert!(matches!(tabs[0].layout, ViewLayout::Split { .. }));
    }

    fn split_ratio(layout: &ViewLayout) -> u16 {
        match layout {
            ViewLayout::Split { ratio, .. } => *ratio,
            ViewLayout::Pane { .. } => panic!("expected split layout"),
        }
    }

    #[test]
    fn resize_command_supports_relative_and_absolute_widths() {
        let mut app = app(2);
        app.split_view(Some("vertical"));
        app.pane_areas = layout_areas(&app.active_tab().layout, Rect::new(0, 0, 100, 20));

        app.resize_command(Some("width +10"));
        assert_eq!(split_ratio(&app.active_tab().layout), 400);

        app.pane_areas = layout_areas(&app.active_tab().layout, Rect::new(0, 0, 100, 20));
        app.resize_command(Some("width 30"));
        assert_eq!(split_ratio(&app.active_tab().layout), 700);
    }

    #[test]
    fn resize_clamps_both_views_to_minimum_size() {
        let mut app = app(2);
        app.split_view(Some("vertical"));
        app.pane_areas = layout_areas(&app.active_tab().layout, Rect::new(0, 0, 100, 20));

        app.resize_command(Some("width +100"));

        let areas = layout_areas(&app.active_tab().layout, Rect::new(0, 0, 100, 20));
        assert_eq!(areas[0].1.width, MIN_VIEW_WIDTH);
        assert_eq!(areas[1].1.width, 100 - MIN_VIEW_WIDTH);
    }

    #[test]
    fn resize_uses_nearest_matching_split_and_equalizes_leaf_views() {
        let mut app = app(3);
        app.split_view(Some("vertical"));
        app.split_view(Some("vertical"));
        app.pane_areas = layout_areas(&app.active_tab().layout, Rect::new(0, 0, 100, 20));

        app.resize_command(Some("width +5"));

        let ViewLayout::Split {
            ratio: outer_ratio,
            second,
            ..
        } = &app.active_tab().layout
        else {
            panic!("expected outer split");
        };
        assert_eq!(*outer_ratio, EQUAL_SPLIT_RATIO);
        assert_eq!(split_ratio(second), 400);

        app.resize_command(Some("equal"));
        let ViewLayout::Split { ratio, second, .. } = &app.active_tab().layout else {
            panic!("expected outer split");
        };
        assert_eq!(*ratio, 333);
        assert_eq!(split_ratio(second), EQUAL_SPLIT_RATIO);
    }

    #[test]
    fn resize_equal_gives_repeated_horizontal_splits_equal_heights() {
        let mut app = app(4);
        app.split_view(Some("horizontal"));
        app.split_view(Some("horizontal"));
        app.split_view(Some("horizontal"));

        app.resize_command(Some("equal"));

        let areas = layout_areas(&app.active_tab().layout, Rect::new(0, 0, 80, 40));
        assert_eq!(
            areas
                .iter()
                .map(|(_, area)| area.height)
                .collect::<Vec<_>>(),
            vec![10, 10, 10, 10]
        );
    }

    #[test]
    fn ctrl_w_resize_shortcuts_adjust_active_view() {
        let mut app = app(2);
        app.split_view(Some("horizontal"));
        app.pane_areas = layout_areas(&app.active_tab().layout, Rect::new(0, 0, 80, 20));

        app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        app.handle_key(key(KeyCode::Char('+')));

        assert_eq!(split_ratio(&app.active_tab().layout), 450);
    }

    #[test]
    fn resize_reports_when_no_matching_split_exists() {
        let mut app = app(2);
        app.split_view(Some("vertical"));
        app.pane_areas = layout_areas(&app.active_tab().layout, Rect::new(0, 0, 100, 20));

        app.resize_command(Some("height +1"));

        assert_eq!(
            app.message.as_deref(),
            Some("Active view has no resizable horizontal split")
        );
    }

    #[test]
    fn new_selects_first_workflow() {
        assert_eq!(app(2).active_tab().table_state.selected(), Some(0));
    }

    #[test]
    fn new_leaves_selection_empty_without_workflows() {
        assert_eq!(app(0).active_tab().table_state.selected(), None);
    }

    #[test]
    fn new_loading_returns_before_workflows_are_available() {
        let mut app = App::new_loading(
            "owner/repository".parse().unwrap(),
            None,
            None,
            Arc::new(TestWorkflowSource),
            None,
            0,
        );

        assert!(app.is_loading());
        assert!(app.workflows.is_empty());
        assert_eq!(
            app.normal_status(),
            (
                " LOADING ",
                "Loading workflows for owner/repository...".to_owned()
            )
        );

        complete_loading(&mut app);

        assert!(!app.is_loading());
        assert_eq!(app.workflows.len(), 100);
        assert_eq!(app.active_tab().table_state.selected(), Some(0));
    }

    #[test]
    fn initial_load_displays_workflows_before_statuses_finish() {
        let mut app = App::new_loading(
            "owner/repository".parse().unwrap(),
            None,
            None,
            Arc::new(ProgressiveWorkflowSource),
            None,
            0,
        );
        let discovered = app
            .load_receiver
            .as_ref()
            .unwrap()
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        app.handle_load_event(discovered);

        assert!(app.is_loading());
        assert_eq!(app.workflows.len(), 2);
        assert_eq!(app.workflows[0].run_status, RunStatus::Other);
        assert_eq!(app.active_tab().table_state.selected(), Some(0));
        assert_eq!(
            app.normal_status().1,
            "Loading run status for owner/repository... (0/2)"
        );
        assert_eq!(app.loading_status_ids, BTreeSet::from([1, 2]));

        complete_loading(&mut app);

        assert!(!app.is_loading());
        assert!(
            app.workflows
                .iter()
                .all(|workflow| workflow.run_status == RunStatus::Success)
        );
        assert!(app.loading_status_ids.is_empty());
    }

    #[test]
    fn refresh_progress_counts_only_displayed_workflows() {
        let source = Arc::new(RecordingWorkflowSource {
            discovered_ids: vec![1, 2, 3, 4, 5],
            loaded_ids: std::sync::Mutex::new(Vec::new()),
        });
        let mut app = App::new(
            "owner/repository".parse().unwrap(),
            vec![workflow(2), workflow(4)],
            None,
            Arc::clone(&source) as Arc<dyn WorkflowSource>,
            None,
        );

        app.refresh_workflows();
        complete_loading(&mut app);

        let mut loaded = source.loaded_ids.lock().unwrap().clone();
        loaded.sort_unstable();
        assert_eq!(loaded, vec![2, 4]);
        assert_eq!(app.loaded_status_count, 2);
        assert_eq!(app.workflows.len(), 2);
    }

    #[test]
    fn refresh_progress_never_exceeds_total_while_statuses_stream_in() {
        let source = Arc::new(RecordingWorkflowSource {
            discovered_ids: vec![1, 2, 3, 4, 5],
            loaded_ids: std::sync::Mutex::new(Vec::new()),
        });
        let mut app = App::new(
            "owner/repository".parse().unwrap(),
            vec![workflow(2), workflow(4)],
            None,
            Arc::clone(&source) as Arc<dyn WorkflowSource>,
            None,
        );

        app.refresh_workflows();
        while app.is_loading() {
            let event = app
                .load_receiver
                .as_ref()
                .unwrap()
                .recv_timeout(Duration::from_secs(1))
                .unwrap();
            app.handle_load_event(event);
            assert!(
                app.loaded_status_count <= app.workflows.len(),
                "progress {} exceeded total {}",
                app.loaded_status_count,
                app.workflows.len()
            );
            if app.is_loading() && !app.workflows.is_empty() {
                assert_eq!(
                    app.normal_status().1,
                    format!(
                        "Refreshing run status for owner/repository... ({}/2)",
                        app.loaded_status_count
                    )
                );
            }
        }
    }

    #[test]
    fn refresh_keeps_previous_status_visible_instead_of_marking_rows_loading() {
        let mut app = app(2);
        app.workflows[0].run_status = RunStatus::Failure;
        app.workflows[0].run_metrics.last_24_hours = RunCounts {
            passed: 3,
            failed: 1,
            total: 4,
        };
        app.workflows[1].run_status = RunStatus::Success;

        app.refresh_workflows();
        let discovered = app
            .load_receiver
            .as_ref()
            .unwrap()
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        app.handle_load_event(discovered);

        assert!(app.is_loading());
        assert!(app.loading_status_ids.is_empty());
        assert_eq!(app.workflows[0].run_status, RunStatus::Failure);
        assert_eq!(
            app.workflows[0].run_metrics.last_24_hours,
            RunCounts {
                passed: 3,
                failed: 1,
                total: 4
            }
        );
        assert_eq!(app.workflows[1].run_status, RunStatus::Success);

        complete_loading(&mut app);

        assert!(app.loading_status_ids.is_empty());
        assert_eq!(app.workflows[0].name, "Refreshed Workflow 0");
    }

    #[test]
    fn loading_saved_view_filters_current_data_in_saved_order() {
        let mut app = App::new_loading(
            "owner/repository".parse().unwrap(),
            Some(vec![3, 1]),
            Some(PathBuf::from("saved-view.json")),
            Arc::new(TestWorkflowSource),
            None,
            0,
        );

        complete_loading(&mut app);

        let ids: Vec<_> = app.workflows.iter().map(|workflow| workflow.id).collect();
        assert_eq!(ids, vec![3, 1]);
        assert_eq!(app.state_path, Some(PathBuf::from("saved-view.json")));
    }

    #[test]
    fn persisted_tabs_exclude_triage_views_and_remap_active_tab() {
        let mut app = app(3);
        let mut triage = app.active_tab().clone();
        triage.name = Some("Triage".to_owned());
        triage.is_triage = true;
        app.tabs.push(triage);
        app.active_tab = 1;

        let (tabs, active_tab) = app.persisted_tabs();

        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs[0].name, None);
        assert_eq!(tabs[0].workflow_ids, vec![0, 1, 2]);
        assert_eq!(active_tab, 0);
    }

    #[test]
    fn persisted_tabs_fall_back_to_global_workflows_when_only_triage_remains() {
        let mut app = app(3);
        app.tabs[0].is_triage = true;
        app.tabs[0].workflow_ids = vec![2];

        let (tabs, active_tab) = app.persisted_tabs();

        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs[0].workflow_ids, vec![0, 1, 2]);
        assert_eq!(tabs[0].selected_workflow_id, Some(0));
        assert_eq!(active_tab, 0);
    }

    #[test]
    fn refresh_command_updates_current_workflows_in_background() {
        let mut app = app(2);
        app.workflows[0].name = "Stale Workflow".to_owned();
        app.select_next(1);

        enter_command(&mut app, "refresh");
        app.handle_key(key(KeyCode::Enter));

        assert!(app.is_loading());
        assert_eq!(app.pending_load.as_ref().unwrap().kind, LoadKind::Refresh);
        assert_eq!(app.normal_status().0, " REFRESHING ");
        complete_loading(&mut app);

        assert_eq!(app.workflows[0].name, "Refreshed Workflow 0");
        assert_eq!(app.active_tab().table_state.selected(), Some(1));
        assert_eq!(app.message.as_deref(), Some("2 workflows refreshed"));
    }

    #[test]
    fn refresh_starts_automatically_when_interval_elapses() {
        let mut app = app(2);
        app.next_refresh = Instant::now() - Duration::from_millis(1);

        app.refresh_if_due();

        assert!(app.is_loading());
        assert_eq!(app.pending_load.as_ref().unwrap().kind, LoadKind::Refresh);
        complete_loading(&mut app);
    }

    #[test]
    fn refresh_rate_command_changes_interval_and_footer() {
        let mut app = app(1);

        enter_command(&mut app, "refresh-rate 30");
        app.handle_key(key(KeyCode::Enter));

        assert_eq!(app.refresh_interval, Duration::from_secs(30));
        assert_eq!(
            app.message.as_deref(),
            Some("Refresh rate set to 30 seconds")
        );
        assert_eq!(app.refresh_rate_label(), "refresh: 30s");
    }

    #[test]
    fn refresh_rate_command_rejects_missing_invalid_and_zero_values() {
        let mut app = app(1);

        enter_command(&mut app, "refresh-rate");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.message.as_deref(), Some("E471: Argument required"));

        enter_command(&mut app, "refresh-rate fast");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.message.as_deref(), Some("E474: Invalid argument: fast"));

        enter_command(&mut app, "refresh-rate 0");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            app.message.as_deref(),
            Some("E474: Refresh rate must be greater than zero")
        );
        assert_eq!(app.refresh_interval, DEFAULT_REFRESH_INTERVAL);
    }

    #[test]
    fn handle_key_moves_selection_and_clamps_to_bounds() {
        let mut app = app(20);

        app.handle_key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT));
        assert_eq!(app.active_tab().table_state.selected(), Some(19));

        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(app.active_tab().table_state.selected(), Some(19));

        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(app.active_tab().table_state.selected(), Some(9));

        app.handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
        assert_eq!(app.active_tab().table_state.selected(), Some(0));
    }

    #[test]
    fn handle_key_supports_gg_to_select_first_workflow() {
        let mut app = app(3);
        app.select_last();

        app.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE));

        assert_eq!(app.active_tab().table_state.selected(), Some(0));
    }

    #[test]
    fn handle_key_does_not_quit_from_normal_mode() {
        let mut app = app(1);

        app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert!(!app.should_quit);
    }

    #[test]
    fn handle_key_quits_after_q_command() {
        let mut app = app(1);

        app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(app.should_quit);
    }

    #[test]
    fn escape_cancels_command_mode() {
        let mut app = app(1);

        app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert_eq!(app.mode, Mode::Normal);
        assert!(app.command.is_empty());
        assert!(!app.should_quit);
    }

    #[test]
    fn unknown_command_displays_error_and_returns_to_normal_mode() {
        let mut app = app(1);

        enter_command(&mut app, "x");
        app.handle_key(key(KeyCode::Enter));

        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(
            app.message.as_deref(),
            Some("E492: Not an editor command: x")
        );
    }

    #[test]
    fn command_cursor_supports_movement_and_mid_line_editing() {
        let mut app = app(1);
        enter_command(&mut app, "ac");

        app.handle_key(key(KeyCode::Left));
        app.handle_key(key(KeyCode::Char('b')));
        assert_eq!(app.command, "abc");
        assert_eq!(app.command_cursor, 2);

        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(app.command, "ac");
        assert_eq!(app.command_cursor, 1);

        app.handle_key(key(KeyCode::Delete));
        assert_eq!(app.command, "a");
        assert_eq!(app.command_cursor, 1);
    }

    #[test]
    fn command_history_scrolls_and_restores_draft() {
        let mut app = app(1);
        for command in ["first", "second"] {
            enter_command(&mut app, command);
            app.handle_key(key(KeyCode::Enter));
        }
        enter_command(&mut app, "draft");

        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.command, "second");
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.command, "first");
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.command, "first");

        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.command, "second");
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.command, "draft");
        assert_eq!(app.history_index, None);
        assert_eq!(app.command_cursor, 5);
    }

    #[test]
    fn command_history_ignores_empty_commands() {
        let mut app = app(1);
        enter_command(&mut app, "");
        app.handle_key(key(KeyCode::Enter));

        assert!(app.command_history.is_empty());
    }

    #[test]
    fn command_history_persists_and_suppresses_consecutive_duplicates() {
        let path = env::temp_dir().join(format!(
            "gh-actui-command-history-{}.txt",
            std::process::id()
        ));
        let mut app = app(1);
        app.command_history_path = Some(path.clone());

        for command in [
            "filter status:failure",
            "filter status:failure",
            "sort name",
        ] {
            enter_command(&mut app, command);
            app.handle_key(key(KeyCode::Enter));
        }

        assert_eq!(app.command_history, ["filter status:failure", "sort name"]);
        assert_eq!(
            load_command_history(&path).unwrap(),
            ["filter status:failure", "sort name"]
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn command_history_load_retains_latest_entries() {
        let path = env::temp_dir().join(format!(
            "gh-actui-command-history-limit-{}.txt",
            std::process::id()
        ));
        let contents = (0..MAX_COMMAND_HISTORY + 2)
            .map(|index| format!("command-{index}"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&path, contents).unwrap();

        let history = load_command_history(&path).unwrap();

        assert_eq!(history.len(), MAX_COMMAND_HISTORY);
        assert_eq!(history.first().unwrap(), "command-2");
        assert_eq!(
            history.last().unwrap(),
            &format!("command-{}", MAX_COMMAND_HISTORY + 1)
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn filter_command_applies_and_clears_visible_view() {
        let mut app = app(3);

        enter_command(&mut app, "filter name:Workflow*");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.visible_workflows().len(), 3);
        assert_eq!(app.active_tab().filter.as_deref(), Some("name:Workflow*"));

        enter_command(&mut app, "filter name:Workflow\\ 1");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.visible_workflows().len(), 1);
        assert_eq!(app.visible_workflows()[0].id, 1);

        enter_command(&mut app, "filter");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.visible_workflows().len(), 3);
        assert!(app.active_tab().filter.is_none());
    }

    #[test]
    fn sort_command_applies_direction_and_clears() {
        let mut app = app(3);
        app.workflows[0].run_metrics.last_24_hours = RunCounts {
            passed: 1,
            failed: 9,
            total: 10,
        };
        app.workflows[1].run_metrics.last_24_hours = RunCounts {
            passed: 9,
            failed: 1,
            total: 10,
        };

        enter_command(&mut app, "sort 24h.rate:desc");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.visible_workflows()[0].id, 1);

        enter_command(&mut app, "sort");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.visible_workflows()[0].id, 0);
        assert!(app.active_tab().sort.is_none());
    }

    #[test]
    fn filter_and_sort_commands_report_parse_errors_without_changing_view() {
        let mut app = app(2);

        enter_command(&mut app, "filter name:\"unterminated");
        app.handle_key(key(KeyCode::Enter));
        assert!(app.active_tab().filter.is_none());
        assert!(
            app.message
                .as_deref()
                .unwrap()
                .contains("unterminated quote")
        );

        enter_command(&mut app, "sort name:sideways");
        app.handle_key(key(KeyCode::Enter));
        assert!(app.active_tab().sort.is_none());
        assert!(
            app.message
                .as_deref()
                .unwrap()
                .contains("invalid sort direction")
        );
    }

    #[test]
    fn write_and_edit_commands_remember_path_and_restore_view() {
        let path = std::env::temp_dir().join(format!(
            "gh-actui-command-state-{}.json",
            std::process::id()
        ));
        let mut app = app(2);

        enter_command(&mut app, "filter status:success");
        app.handle_key(key(KeyCode::Enter));
        enter_command(&mut app, "sort name:desc");
        app.handle_key(key(KeyCode::Enter));
        enter_command(&mut app, &format!("w {}", path.display()));
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.state_path.as_ref(), Some(&path));
        assert!(path.is_file());

        app.workflows.clear();
        enter_command(&mut app, "e");
        app.handle_key(key(KeyCode::Enter));
        assert!(app.is_loading());
        complete_loading(&mut app);
        std::fs::remove_file(path).unwrap();

        assert_eq!(app.workflows.len(), 2);
        assert_eq!(app.workflows[0].name, "Refreshed Workflow 0");
        assert_eq!(app.workflows[0].run_status, RunStatus::Success);
        assert_eq!(app.active_tab().filter.as_deref(), Some("status:success"));
        assert_eq!(app.active_tab().sort.as_deref(), Some("name:desc"));
        assert_eq!(app.active_tab().table_state.selected(), Some(0));
    }

    #[test]
    fn write_command_without_path_reports_error() {
        let mut app = app(1);

        enter_command(&mut app, "w");
        app.handle_key(key(KeyCode::Enter));

        assert_eq!(app.message.as_deref(), Some("E32: No file name"));
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn write_quit_command_saves_then_quits() {
        let path = std::env::temp_dir().join(format!(
            "gh-actui-write-quit-state-{}.json",
            std::process::id()
        ));
        let mut app = app(2);

        enter_command(&mut app, &format!("wq {}", path.display()));
        app.handle_key(key(KeyCode::Enter));
        let saved = ViewState::load(&path).unwrap();
        std::fs::remove_file(path).unwrap();

        assert!(app.should_quit);
        assert_eq!(saved.workflow_ids, vec![0, 1]);
    }

    #[test]
    fn write_quit_command_does_not_quit_when_write_fails() {
        let mut app = app(1);

        enter_command(&mut app, "wq");
        app.handle_key(key(KeyCode::Enter));

        assert!(!app.should_quit);
        assert_eq!(app.message.as_deref(), Some("E32: No file name"));
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn write_state_persists_all_tabs_and_active_selection() {
        let path =
            std::env::temp_dir().join(format!("gh-actui-tabs-state-{}.json", std::process::id()));
        let mut app = app(3);
        app.select_next(1);
        enter_command(&mut app, "tabnew Failures");
        app.handle_key(key(KeyCode::Enter));
        enter_command(&mut app, "filter name:Workflow\\ 2");
        app.handle_key(key(KeyCode::Enter));
        enter_command(&mut app, &format!("w {}", path.display()));
        app.handle_key(key(KeyCode::Enter));

        let saved = ViewState::load(&path).unwrap();
        std::fs::remove_file(path).unwrap();

        assert_eq!(saved.active_tab, 1);
        assert_eq!(saved.tabs.len(), 2);
        assert_eq!(saved.tabs[0].selected_workflow_id, Some(1));
        assert_eq!(saved.tabs[1].name.as_deref(), Some("Failures"));
        assert_eq!(saved.tabs[1].filter.as_deref(), Some("name:Workflow\\ 2"));
        assert_eq!(saved.tabs[1].selected_workflow_id, Some(2));
    }

    #[test]
    fn delete_command_removes_selected_workflow() {
        let mut app = app(3);
        app.select_next(1);

        enter_command(&mut app, "d");
        app.handle_key(key(KeyCode::Enter));

        let ids = app.active_tab().workflow_ids.clone();
        assert_eq!(ids, vec![0, 2]);
        assert_eq!(app.workflows.len(), 3);
        assert_eq!(app.active_tab().table_state.selected(), Some(1));
        assert_eq!(app.message.as_deref(), Some("1 workflow deleted"));
    }

    #[test]
    fn delete_count_removes_consecutive_workflows_and_clamps_at_end() {
        let mut app = app(5);
        app.select_next(3);

        enter_command(&mut app, "d3");
        app.handle_key(key(KeyCode::Enter));

        let ids = app.active_tab().workflow_ids.clone();
        assert_eq!(ids, vec![0, 1, 2]);
        assert_eq!(app.workflows.len(), 5);
        assert_eq!(app.active_tab().table_state.selected(), Some(2));
        assert_eq!(app.message.as_deref(), Some("2 workflows deleted"));
    }

    #[test]
    fn delete_count_follows_filtered_and_sorted_visible_order() {
        let mut app = app(4);
        enter_command(&mut app, "filter -name:Workflow\\ 0");
        app.handle_key(key(KeyCode::Enter));
        enter_command(&mut app, "sort name:desc");
        app.handle_key(key(KeyCode::Enter));

        enter_command(&mut app, "d2");
        app.handle_key(key(KeyCode::Enter));

        let ids = app.active_tab().workflow_ids.clone();
        assert_eq!(ids, vec![0, 1]);
        assert_eq!(app.visible_workflows()[0].id, 1);
    }

    #[test]
    fn delete_count_can_empty_view_and_clear_selection() {
        let mut app = app(2);

        enter_command(&mut app, "d2");
        app.handle_key(key(KeyCode::Enter));

        assert!(app.active_tab().workflow_ids.is_empty());
        assert_eq!(app.workflows.len(), 2);
        assert_eq!(app.active_tab().table_state.selected(), None);
    }

    #[test]
    fn delete_zero_reports_invalid_range() {
        let mut app = app(2);

        enter_command(&mut app, "d0");
        app.handle_key(key(KeyCode::Enter));

        assert_eq!(app.workflows.len(), 2);
        assert_eq!(app.message.as_deref(), Some("E16: Invalid range"));
    }

    #[test]
    fn delete_from_empty_view_reports_error() {
        let mut app = app(0);

        enter_command(&mut app, "d");
        app.handle_key(key(KeyCode::Enter));

        assert_eq!(app.message.as_deref(), Some("E749: Empty buffer"));
    }

    #[test]
    fn tabnew_duplicates_active_view_without_duplicating_workflows() {
        let mut app = app(3);
        app.select_next(1);
        enter_command(&mut app, "filter -name:Workflow\\ 0");
        app.handle_key(key(KeyCode::Enter));
        enter_command(&mut app, "sort name:desc");
        app.handle_key(key(KeyCode::Enter));

        enter_command(&mut app, "tabnew");
        app.handle_key(key(KeyCode::Enter));

        assert_eq!(app.tabs.len(), 2);
        assert_eq!(app.active_tab, 1);
        assert_eq!(app.workflows.len(), 3);
        assert_eq!(app.tabs[0].workflow_ids, app.tabs[1].workflow_ids);
        assert_eq!(app.tabs[1].filter.as_deref(), Some("-name:Workflow\\ 0"));
        assert_eq!(app.tabs[1].sort.as_deref(), Some("name:desc"));

        enter_command(&mut app, "d");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.tabs[0].workflow_ids, vec![0, 1, 2]);
        assert_eq!(app.tabs[1].workflow_ids, vec![0, 1]);
    }

    #[test]
    fn tab_commands_and_shortcuts_wrap_between_tabs() {
        let mut app = app(2);
        for _ in 0..2 {
            enter_command(&mut app, "tabnew");
            app.handle_key(key(KeyCode::Enter));
        }
        assert_eq!(app.active_tab, 2);

        enter_command(&mut app, "tabnext");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.active_tab, 0);
        enter_command(&mut app, "tabp");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.active_tab, 2);

        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::CONTROL));
        assert_eq!(app.active_tab, 0);
        app.handle_key(KeyEvent::new(
            KeyCode::BackTab,
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ));
        assert_eq!(app.active_tab, 2);

        app.mode = Mode::Command;
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::CONTROL));
        assert_eq!(app.active_tab, 0);
        assert_eq!(app.mode, Mode::Command);
    }

    #[test]
    fn tabclose_closes_active_tab_and_resets_the_last_tab() {
        let mut app = app(2);
        enter_command(&mut app, "tabnew");
        app.handle_key(key(KeyCode::Enter));

        enter_command(&mut app, "tabc");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.tabs.len(), 1);
        assert_eq!(app.active_tab, 0);
        assert_eq!(app.active_tab().workflow_ids, vec![0, 1]);

        enter_command(&mut app, "tabclose");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.tabs.len(), 1);
        assert!(app.active_tab().workflow_ids.is_empty());
        assert!(app.workflows.len() == 2);
    }

    #[test]
    fn tabnew_uses_optional_argument_as_tab_name() {
        let mut app = app(1);

        enter_command(&mut app, "tabnew Weekly failures");
        app.handle_key(key(KeyCode::Enter));

        assert_eq!(app.tabs.len(), 2);
        assert_eq!(app.active_tab().name.as_deref(), Some("Weekly failures"));
        assert_eq!(app.message.as_deref(), Some("Weekly failures opened"));
    }

    #[test]
    fn tabsetname_renames_active_tab_with_full_argument() {
        let mut app = app(1);
        enter_command(&mut app, "tabnew Old name");
        app.handle_key(key(KeyCode::Enter));

        enter_command(&mut app, "tabsetname Weekly Linux failures");
        app.handle_key(key(KeyCode::Enter));

        assert_eq!(
            app.active_tab().name.as_deref(),
            Some("Weekly Linux failures")
        );
        assert_eq!(
            app.message.as_deref(),
            Some("Tab renamed to Weekly Linux failures")
        );
        assert!(app.tabs[0].name.is_none());
    }

    #[test]
    fn tabsetname_requires_a_name() {
        let mut app = app(1);

        enter_command(&mut app, "tabsetname");
        app.handle_key(key(KeyCode::Enter));

        assert!(app.active_tab().name.is_none());
        assert_eq!(app.message.as_deref(), Some("E471: Argument required"));
    }

    #[test]
    fn triage_uses_only_visible_failing_workflows_and_opens_result_tab() {
        let mut app = app(4);
        app.workflows[0].run_status = RunStatus::Failure;
        app.workflows[1].run_status = RunStatus::Success;
        app.workflows[2].run_status = RunStatus::Failure;
        app.workflows[3].run_status = RunStatus::Failure;
        enter_command(&mut app, "filter -name:Workflow\\ 3");
        app.handle_key(key(KeyCode::Enter));

        enter_command(&mut app, "triage");
        app.handle_key(key(KeyCode::Enter));

        assert!(app.triage_receiver.is_some());
        assert_eq!(app.normal_status().0, " TRIAGING ");
        complete_triage(&mut app);

        assert_eq!(app.tabs.len(), 2);
        assert_eq!(app.active_tab().name.as_deref(), Some("Triage"));
        assert!(app.active_tab().is_triage);
        assert_eq!(app.active_tab().workflow_ids, vec![0, 2]);
        assert_eq!(app.triage[&0].failed_jobs, "Job 0");
        assert_eq!(
            app.message.as_deref(),
            Some("2 failing scheduled workflows triaged")
        );
    }

    #[test]
    fn triage_can_run_while_workflow_refresh_is_in_progress() {
        let mut app = app(2);
        app.workflows[0].run_status = RunStatus::Failure;
        app.refresh_workflows();
        assert!(app.is_loading());

        app.start_triage();

        assert!(app.is_loading());
        assert!(app.triage_receiver.is_some());
        complete_loading(&mut app);
        complete_triage(&mut app);

        assert_eq!(app.tabs.len(), 2);
        assert!(app.active_tab().is_triage);
        assert_eq!(app.active_tab().workflow_ids, vec![0]);
    }

    #[test]
    fn refresh_on_triage_tab_reruns_in_place() {
        let mut app = app(3);
        app.workflows[0].run_status = RunStatus::Failure;
        app.workflows[1].run_status = RunStatus::Failure;
        enter_command(&mut app, "triage");
        app.handle_key(key(KeyCode::Enter));
        complete_triage(&mut app);
        assert_eq!(app.tabs.len(), 2);

        app.active_tab_mut().workflow_ids = vec![1];
        app.workflows[1].run_status = RunStatus::Success;
        enter_command(&mut app, "refresh");
        app.handle_key(key(KeyCode::Enter));

        assert!(matches!(
            app.triage_target,
            Some(TriageTarget::RefreshTab { index: 1 })
        ));
        complete_triage(&mut app);

        assert_eq!(app.tabs.len(), 2);
        assert_eq!(app.active_tab, 1);
        assert!(app.active_tab().is_triage);
        assert_eq!(app.active_tab().name.as_deref(), Some("Triage"));
        assert_eq!(app.active_tab().workflow_ids, vec![1]);
        assert_eq!(app.triage[&1].failed_jobs, "Job 1");
    }

    #[test]
    fn triage_reports_when_active_view_has_no_failures() {
        let mut app = app(2);

        enter_command(&mut app, "triage");
        app.handle_key(key(KeyCode::Enter));

        assert!(app.triage_receiver.is_none());
        assert_eq!(
            app.message.as_deref(),
            Some("No failing workflows in the active view")
        );
    }

    #[test]
    fn triage_text_preserves_multiline_lit_summary() {
        let triage = WorkflowTriage {
            workflow_id: 1,
            failed_jobs: "Linux tests".to_owned(),
            failed_steps: "Run HLSL Tests".to_owned(),
            failed_tests: vec!["one.test".to_owned(), "two.test".to_owned()],
            unexpectedly_passed_tests: vec!["flaky.test".to_owned()],
            lit_summary: "Failed Tests (2):\n  Suite :: one.test\n  Suite :: two.test".to_owned(),
        };

        let text = triage_text(Some(&triage));
        let lines = text
            .lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();

        assert_eq!(
            lines,
            [
                "Failed jobs: Linux tests",
                "Failed steps: Run HLSL Tests",
                "",
                "Failed tests:",
                "  - one.test",
                "  - two.test",
                "",
                "Unexpectedly passed tests:",
                "  - flaky.test",
                "",
                "lit summary:",
                "Failed Tests (2):",
                "  Suite :: one.test",
                "  Suite :: two.test"
            ]
        );
        assert_eq!(triage_block_height(&text, 80), 16);
    }

    #[test]
    fn triage_summary_correlates_tests_across_workflows() {
        let workflows = vec![workflow(1), workflow(2), workflow(3)];
        let mut triage = HashMap::new();
        triage.insert(
            1,
            WorkflowTriage {
                workflow_id: 1,
                failed_jobs: String::new(),
                failed_steps: String::new(),
                failed_tests: Vec::new(),
                unexpectedly_passed_tests: Vec::new(),
                lit_summary: "\
Failed Tests (2):
  Suite :: common-failure.test
  Suite :: unique.test
Unexpectedly Passed Tests (1):
  Suite :: common-pass.test"
                    .to_owned(),
            },
        );
        triage.insert(
            2,
            WorkflowTriage {
                workflow_id: 2,
                failed_jobs: String::new(),
                failed_steps: String::new(),
                failed_tests: Vec::new(),
                unexpectedly_passed_tests: Vec::new(),
                lit_summary: "\
Failed Tests (1):
  Suite :: common-failure.test
Unexpectedly Passed Tests (1):
  Suite :: common-pass.test"
                    .to_owned(),
            },
        );
        triage.insert(
            3,
            WorkflowTriage {
                workflow_id: 3,
                failed_jobs: String::new(),
                failed_steps: String::new(),
                failed_tests: Vec::new(),
                unexpectedly_passed_tests: Vec::new(),
                lit_summary: "Failed Tests (1):\n  Suite :: another-unique.test".to_owned(),
            },
        );

        let summary = triage_correlation_summary(&workflows, &triage).to_string();

        assert!(summary.contains("Failed in multiple workflows:"));
        assert!(summary.contains("common-failure.test"));
        assert!(summary.contains("Workflow 1"));
        assert!(summary.contains("Workflow 2"));
        assert!(summary.contains("Unexpectedly passed in multiple workflows:"));
        assert!(summary.contains("common-pass.test"));
        assert!(!summary.contains("unique.test"));
    }

    #[test]
    fn triage_summary_reports_when_no_tests_are_shared() {
        let workflows = vec![workflow(1)];
        let triage = HashMap::from([(
            1,
            WorkflowTriage {
                workflow_id: 1,
                failed_jobs: String::new(),
                failed_steps: String::new(),
                failed_tests: Vec::new(),
                unexpectedly_passed_tests: Vec::new(),
                lit_summary: "Failed Tests (1):\n  Suite :: unique.test".to_owned(),
            },
        )]);

        assert_eq!(
            triage_correlation_summary(&workflows, &triage).to_string(),
            "No tests failed or unexpectedly passed in multiple workflows."
        );
    }

    #[test]
    fn triage_summary_correlates_structured_tests_when_raw_summary_is_missing() {
        let workflows = vec![workflow(1), workflow(2), workflow(3)];
        let triage = HashMap::from([
            (
                1,
                WorkflowTriage {
                    workflow_id: 1,
                    failed_jobs: String::new(),
                    failed_steps: String::new(),
                    failed_tests: vec!["shared-failure.test".to_owned()],
                    unexpectedly_passed_tests: vec!["shared-xpass.test".to_owned()],
                    lit_summary: String::new(),
                },
            ),
            (
                2,
                WorkflowTriage {
                    workflow_id: 2,
                    failed_jobs: String::new(),
                    failed_steps: String::new(),
                    failed_tests: vec!["shared-failure.test".to_owned(), "unique.test".to_owned()],
                    unexpectedly_passed_tests: vec!["shared-xpass.test".to_owned()],
                    lit_summary: String::new(),
                },
            ),
            (
                3,
                WorkflowTriage {
                    workflow_id: 3,
                    failed_jobs: String::new(),
                    failed_steps: String::new(),
                    failed_tests: vec!["another-unique.test".to_owned()],
                    unexpectedly_passed_tests: Vec::new(),
                    lit_summary: String::new(),
                },
            ),
        ]);

        let summary = triage_correlation_summary(&workflows, &triage).to_string();

        assert!(summary.contains("shared-failure.test"));
        assert!(summary.contains("shared-xpass.test"));
        assert!(summary.contains("Workflow 1"));
        assert!(summary.contains("Workflow 2"));
        assert!(!summary.contains("unique.test"));
    }

    #[test]
    fn status_indicator_reflects_last_completed_run() {
        let mut workflow = workflow(1);

        assert_eq!(status_indicator(&workflow, true), "⚪");
        workflow.run_status = RunStatus::Success;
        assert_eq!(status_indicator(&workflow, true), "🟢");
        workflow.run_status = RunStatus::Failure;
        assert_eq!(status_indicator(&workflow, true), "🔴");
    }

    #[test]
    fn status_indicator_flashes_while_run_is_in_progress() {
        let mut workflow = workflow(1);
        workflow.run_status = RunStatus::Success;
        workflow.is_in_progress = true;

        assert_eq!(status_indicator(&workflow, true), "🟢");
        assert_eq!(status_indicator(&workflow, false), "");
        assert!(flash_visible(Duration::from_millis(499)));
        assert!(!flash_visible(Duration::from_millis(500)));
        assert!(flash_visible(Duration::from_millis(1000)));
    }

    #[test]
    fn metrics_color_uses_requested_thresholds() {
        assert_eq!(metrics_color(69.9), Color::Red);
        assert_eq!(metrics_color(70.0), Color::Yellow);
        assert_eq!(metrics_color(89.9), Color::Yellow);
        assert_eq!(metrics_color(90.0), Color::Green);
    }
}
