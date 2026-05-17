mod theme;
use theme::{Brand, TREE_BRANCH, TREE_LAST, Theme};

use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use git2::{Repository, StatusOptions};
use ratatui::{
    Frame, Terminal,
    backend::{Backend, CrosstermBackend},
    layout::{Constraint, Direction, Layout},
    style::Modifier,
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};
use std::{error::Error, io, path::PathBuf, sync::mpsc, thread, time::SystemTime};
use walkdir::WalkDir;

use serde::Deserialize;

const MASTER_DIR: &str = "..";
const DEFAULT_GPG_KEY: &str = "417CAB41FAB91318";

#[derive(Clone, Default)]
struct GitHubStats {
    stars: u32,
    forks: u32,
    open_issues: u32,
    url: String,
}

#[derive(Deserialize)]
struct GitHubRepoResponse {
    stargazers_count: u32,
    forks_count: u32,
    open_issues_count: u32,
    html_url: String,
}

fn fetch_github_stats(remote_url: &str) -> Option<GitHubStats> {
    let mut owner_repo = None;

    if remote_url.contains("github.com") {
        if remote_url.starts_with("git@") {
            let parts: Vec<&str> = remote_url.split(':').collect();
            if parts.len() == 2 {
                owner_repo = Some(parts[1].trim_end_matches(".git").to_string());
            }
        } else if remote_url.starts_with("http") {
            if let Some(path) = remote_url.split("github.com/").nth(1) {
                owner_repo = Some(path.trim_end_matches(".git").to_string());
            }
        }
    }

    if let Some(repo_path) = owner_repo {
        let api_url = format!("https://api.github.com/repos/{}", repo_path);
        let mut request = ureq::get(&api_url).header("User-Agent", "Tropical-ProjectManager");

        if let Ok(token) = std::env::var("GITHUB_TOKEN") {
            request = request.header("Authorization", &format!("Bearer {}", token));
        }

        if let Ok(mut response) = request.call() {
            if let Ok(data) = response.body_mut().read_json::<GitHubRepoResponse>() {
                return Some(GitHubStats {
                    stars: data.stargazers_count,
                    forks: data.forks_count,
                    open_issues: data.open_issues_count,
                    url: data.html_url,
                });
            }
        }
    }
    None
}

#[derive(Clone)]
enum FileStatusType {
    Modified,
    Untracked,
    Deleted,
}

#[derive(Clone)]
struct GpgStatus {
    is_configured: bool,
    signing_key: Option<String>,
    auto_retrieve: bool,
}

struct Project {
    name: String,
    path: PathBuf,
    current_branch: String,
    is_dirty: bool,
    untracked: usize,
    modified: usize,
    deleted: usize,
    changed_files: Vec<(String, FileStatusType)>,
    ahead: usize,
    behind: usize,
    last_commit_msg: Option<String>,
    last_commit_time: i64,
    github_stats: Option<GitHubStats>,
    gpg_status: GpgStatus,
    remote_status: Option<bool>,
}

enum InputMode {
    Normal,
    CreatingProject,
    ConfigGpgKey,
    ConfirmAction,
}

enum ActionMode {
    None,
    ConfigureGpg,
    TestRemotes,
}

struct ConfirmData {
    action: ActionMode,
    message: String,
    projects: Vec<String>,
}

struct App {
    projects: Vec<Project>,
    list_state: ListState,
    input_mode: InputMode,
    input_buffer: String,
    is_loading: bool,
    rx: Option<mpsc::Receiver<Vec<Project>>>,
    tick: u8,
    theme: Theme,
    action_mode: ActionMode,
    confirm_data: Option<ConfirmData>,
    global_gpg_key: String,
    status_message: Option<String>,
}

impl App {
    fn new(brand: Brand) -> App {
        let mut app = App {
            projects: Vec::new(),
            list_state: ListState::default(),
            input_mode: InputMode::Normal,
            input_buffer: String::new(),
            is_loading: false,
            rx: None,
            tick: 0,
            theme: Theme::from_brand(brand),
            action_mode: ActionMode::None,
            confirm_data: None,
            global_gpg_key: DEFAULT_GPG_KEY.to_string(),
            status_message: None,
        };
        app.scan_projects();
        app
    }

    fn scan_projects(&mut self) {
        if self.is_loading {
            return;
        }
        self.is_loading = true;
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        let gpg_key = self.global_gpg_key.clone();

        thread::spawn(move || {
            let mut found_projects = Vec::new();
            let walker = WalkDir::new(MASTER_DIR)
                .min_depth(1)
                .max_depth(2)
                .into_iter();

            for entry in walker.filter_map(Result::ok) {
                let path = entry.path();
                if path.is_dir() {
                    let git_dir = path.join(".git");
                    if git_dir.exists() {
                        if let Ok(repo) = Repository::open(path) {
                            let name = path
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .to_string();

                            let mut current_branch = String::from("unknown");
                            let mut last_commit_msg = None;
                            let mut last_commit_time = 0;
                            let mut ahead = 0;
                            let mut behind = 0;

                            if let Ok(head) = repo.head() {
                                if let Some(branch_name) = head.shorthand() {
                                    current_branch = branch_name.to_string();
                                }
                                if let Ok(commit) = head.peel_to_commit() {
                                    last_commit_time = commit.time().seconds();
                                    if let Some(msg) = commit.summary() {
                                        last_commit_msg = Some(msg.to_string());
                                    }
                                }
                                if head.is_branch() {
                                    if let Some(branch_name) = head.shorthand() {
                                        if let Ok(branch) =
                                            repo.find_branch(branch_name, git2::BranchType::Local)
                                        {
                                            if let Ok(upstream) = branch.upstream() {
                                                if let (Some(local_oid), Some(upstream_oid)) =
                                                    (branch.get().target(), upstream.get().target())
                                                {
                                                    if let Ok((a, b)) = repo
                                                        .graph_ahead_behind(local_oid, upstream_oid)
                                                    {
                                                        ahead = a;
                                                        behind = b;
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }

                            let mut untracked = 0;
                            let mut modified = 0;
                            let mut deleted = 0;
                            let mut changed_files = Vec::new();
                            let mut status_opts = StatusOptions::new();
                            status_opts.include_untracked(true);

                            if let Ok(statuses) = repo.statuses(Some(&mut status_opts)) {
                                for s in statuses.iter() {
                                    let status = s.status();
                                    let path_str = s.path().unwrap_or("").to_string();

                                    if status
                                        .intersects(git2::Status::WT_NEW | git2::Status::INDEX_NEW)
                                    {
                                        untracked += 1;
                                        if changed_files.len() < 15 {
                                            changed_files.push((
                                                path_str.clone(),
                                                FileStatusType::Untracked,
                                            ));
                                        }
                                    }
                                    if status.intersects(
                                        git2::Status::WT_MODIFIED
                                            | git2::Status::INDEX_MODIFIED
                                            | git2::Status::WT_RENAMED
                                            | git2::Status::INDEX_RENAMED,
                                    ) {
                                        modified += 1;
                                        if changed_files.len() < 15 {
                                            changed_files
                                                .push((path_str.clone(), FileStatusType::Modified));
                                        }
                                    }
                                    if status.intersects(
                                        git2::Status::WT_DELETED | git2::Status::INDEX_DELETED,
                                    ) {
                                        deleted += 1;
                                        if changed_files.len() < 15 {
                                            changed_files
                                                .push((path_str.clone(), FileStatusType::Deleted));
                                        }
                                    }
                                }
                            }

                            let is_dirty = (untracked + modified + deleted) > 0;

                            let mut github_stats = None;
                            let mut remote_status = None;
                            if let Ok(mut remote) = repo.find_remote("origin") {
                                if let Some(url) = remote.url() {
                                    github_stats = fetch_github_stats(url);
                                    if let Ok(_) = remote.connect(git2::Direction::Fetch) {
                                        remote_status = Some(true);
                                    }
                                }
                            }

                            let gpg_status = check_gpg_status(&git_dir, &gpg_key);

                            found_projects.push(Project {
                                name,
                                path: std::fs::canonicalize(path)
                                    .unwrap_or_else(|_| path.to_path_buf()),
                                current_branch,
                                is_dirty,
                                untracked,
                                modified,
                                deleted,
                                changed_files,
                                ahead,
                                behind,
                                last_commit_msg,
                                last_commit_time,
                                github_stats,
                                gpg_status,
                                remote_status,
                            });
                        }
                    }
                }
            }

            found_projects.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
            let _ = tx.send(found_projects);
        });
    }

    fn next(&mut self) {
        if self.projects.is_empty() {
            return;
        }
        let i = match self.list_state.selected() {
            Some(i) => {
                if i >= self.projects.len().saturating_sub(1) {
                    0
                } else {
                    i + 1
                }
            }
            None => 0,
        };
        self.list_state.select(Some(i));
    }

    fn previous(&mut self) {
        if self.projects.is_empty() {
            return;
        }
        let i = match self.list_state.selected() {
            Some(i) => {
                if i == 0 {
                    self.projects.len().saturating_sub(1)
                } else {
                    i - 1
                }
            }
            None => 0,
        };
        self.list_state.select(Some(i));
    }

    fn select_index(&mut self, index: usize) {
        if index < self.projects.len() {
            self.list_state.select(Some(index));
        }
    }

    fn configure_gpg_all(&mut self, key: &str) {
        let key_owned = key.to_string();

        thread::spawn(move || {
            let walker = WalkDir::new(MASTER_DIR)
                .min_depth(1)
                .max_depth(2)
                .into_iter();

            for entry in walker.filter_map(Result::ok) {
                let path = entry.path();
                if path.is_dir() && path.join(".git").exists() {
                    if let Ok(repo) = Repository::open(path) {
                        let _ = repo.config().and_then(|mut cfg| {
                            cfg.set_i32("commit.gpgsign", 1)?;
                            cfg.set_str("user.signingkey", &key_owned)?;
                            cfg.set_str("gpg.program", "gpg")?;
                            cfg.set_i32("gpg.autoKeyRetrieve", 1)?;
                            Ok(())
                        });
                    }
                }
            }
        });

        self.status_message = Some(format!("Configuring GPG key {} on all repos...", key));
        self.scan_projects();
    }

    fn test_remotes(&mut self) {
        self.status_message = Some("Testing remote connections...".to_string());
        self.scan_projects();
    }
}

fn check_gpg_status(git_dir: &PathBuf, expected_key: &str) -> GpgStatus {
    let config_path = git_dir.join("config");
    if !config_path.exists() {
        return GpgStatus {
            is_configured: false,
            signing_key: None,
            auto_retrieve: false,
        };
    }

    if let Ok(content) = std::fs::read_to_string(config_path) {
        let has_signing_key = content.contains(&format!("signingkey = {}", expected_key));
        let has_gpgsign = content.contains("gpgsign = 1");
        let has_auto_retrieve = content.contains("autoKeyRetrieve = true") 
            || content.contains("autoKeyRetrieve = 1");

        return GpgStatus {
            is_configured: has_signing_key && has_gpgsign,
            signing_key: if has_signing_key { Some(expected_key.to_string()) } else { None },
            auto_retrieve: has_auto_retrieve,
        };
    }

    GpgStatus {
        is_configured: false,
        signing_key: None,
        auto_retrieve: false,
    }
}

fn format_time_ago(timestamp: i64) -> String {
    if timestamp == 0 {
        return "unknown".to_string();
    }
    let now = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let diff = now - timestamp;
    if diff < 60 {
        "just now".to_string()
    } else if diff < 3600 {
        format!("{} min ago", diff / 60)
    } else if diff < 86400 {
        format!("{} hr ago", diff / 3600)
    } else if diff < 2592000 {
        format!("{} days ago", diff / 86400)
    } else if diff < 31536000 {
        format!("{} mo ago", diff / 2592000)
    } else {
        format!("{} yr ago", diff / 31536000)
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let brand = match std::env::var("TROPICAL_BRAND").as_deref() {
        Ok("mango") => Brand::Mango,
        Ok("ocean") => Brand::Ocean,
        Ok("pitahaya") => Brand::Pitahaya,
        Ok("papaya") => Brand::Papaya,
        Ok("balandra") => Brand::Balandra,
        _ => Brand::Tropical,
    };

    let mut app = App::new(brand);

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let res = run_app(&mut terminal, &mut app);

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    if let Err(err) = res {
        eprintln!("Application Error: {:?}", err);
    }
    Ok(())
}

fn run_app<B: Backend>(terminal: &mut Terminal<B>, app: &mut App) -> Result<(), Box<dyn Error>>
where
    B::Error: 'static,
{
    loop {
        if let Some(rx) = &app.rx {
            if let Ok(projects) = rx.try_recv() {
                app.projects = projects;
                app.is_loading = false;
                app.rx = None;
                if app.list_state.selected().is_none() && !app.projects.is_empty() {
                    app.list_state.select(Some(0));
                }
            }
        }

        terminal.draw(|f| ui(f, app))?;

        if event::poll(std::time::Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(key) => {
                    if key.kind == event::KeyEventKind::Press {
                        match app.input_mode {
                            InputMode::Normal => match key.code {
                                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                                KeyCode::Down | KeyCode::Char('j') => app.next(),
                                KeyCode::Up | KeyCode::Char('k') => app.previous(),
                                KeyCode::Char('r') => app.scan_projects(),
                                KeyCode::Char('c') => {
                                    app.input_mode = InputMode::CreatingProject;
                                    app.input_buffer.clear();
                                }
                                KeyCode::Char('g') => {
                                    app.input_mode = InputMode::ConfigGpgKey;
                                    app.input_buffer = app.global_gpg_key.clone();
                                }
                                KeyCode::Char('t') => {
                                    app.test_remotes();
                                }
                                _ => {}
                            },
                            InputMode::CreatingProject => match key.code {
                                KeyCode::Enter => {
                                    let name = app.input_buffer.trim().to_string();
                                    if !name.is_empty() {
                                        let new_path = PathBuf::from(MASTER_DIR).join(&name);
                                        let template =
                                            PathBuf::from("./jules_dev_standard/template");
                                        if !new_path.exists() && template.exists() {
                                            if std::fs::create_dir_all(&new_path).is_ok() {
                                                let mut opts = fs_extra::dir::CopyOptions::new();
                                                opts.content_only = true;
                                                let _ = fs_extra::dir::copy(
                                                    &template, &new_path, &opts,
                                                );
                                                let _ = Repository::init(&new_path);
                                            }
                                        }
                                        app.scan_projects();
                                    }
                                    app.input_mode = InputMode::Normal;
                                }
                                KeyCode::Char(c) => app.input_buffer.push(c),
                                KeyCode::Backspace => {
                                    app.input_buffer.pop();
                                }
                                KeyCode::Esc => app.input_mode = InputMode::Normal,
                                _ => {}
                            },
                            InputMode::ConfigGpgKey => match key.code {
                                KeyCode::Enter => {
                                    let key = app.input_buffer.trim().to_string();
                                    if !key.is_empty() {
                                        app.global_gpg_key = key.clone();
                                        app.configure_gpg_all(&key);
                                    }
                                    app.input_mode = InputMode::Normal;
                                }
                                KeyCode::Char(c) => app.input_buffer.push(c),
                                KeyCode::Backspace => {
                                    app.input_buffer.pop();
                                }
                                KeyCode::Esc => app.input_mode = InputMode::Normal,
                                _ => {}
                            },
                            InputMode::ConfirmAction => match key.code {
                                KeyCode::Char('y') | KeyCode::Enter => {
                                    if let Some(confirm) = &app.confirm_data {
                                        let gpg_key = app.global_gpg_key.clone();
                                        match confirm.action {
                                            ActionMode::ConfigureGpg => {
                                                app.configure_gpg_all(&gpg_key);
                                            }
                                            ActionMode::TestRemotes => {
                                                app.test_remotes();
                                            }
                                            ActionMode::None => {}
                                        }
                                    }
                                    app.confirm_data = None;
                                    app.input_mode = InputMode::Normal;
                                }
                                KeyCode::Char('n') | KeyCode::Esc => {
                                    app.confirm_data = None;
                                    app.input_mode = InputMode::Normal;
                                }
                                _ => {}
                            },
                        }
                    }
                }
                Event::Mouse(m) => match m.kind {
                    event::MouseEventKind::ScrollDown => {
                        if let InputMode::Normal = app.input_mode {
                            app.next();
                        }
                    }
                    event::MouseEventKind::ScrollUp => {
                        if let InputMode::Normal = app.input_mode {
                            app.previous();
                        }
                    }
                    event::MouseEventKind::Down(event::MouseButton::Left) => {
                        if let InputMode::Normal = app.input_mode {
                            let y = m.row as usize;
                            if y > 1 {
                                app.select_index(app.list_state.offset() + (y - 2));
                            }
                        }
                    }
                    _ => {}
                },
                _ => {}
            }
        } else {
            app.tick = app.tick.wrapping_add(1);
        }
    }
}

fn ui(f: &mut Frame, app: &mut App) {
    let t = &app.theme;
    let area = f.area();

    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(area);

    let repo_count = format!("{} REPOS", app.projects.len());
    let gpg_key_short = if app.global_gpg_key.len() > 8 {
        &app.global_gpg_key[app.global_gpg_key.len() - 8..]
    } else {
        &app.global_gpg_key
    };
    
    let status_line = if app.is_loading {
        let spinner = t.spinner_span(app.tick);
        let mut spans = t.status_seg("MODE", "SCANNING");
        spans.insert(2, spinner);
        Line::from(spans)
    } else {
        let left = vec![
            ("TROPICAL_PM", "v0.2"),
            ("REPOS", &repo_count),
            ("GPG", gpg_key_short),
        ];
        let right = vec![
            ("R", "REFRESH"),
            ("G", "GPG KEY"),
            ("T", "TEST REMOTE"),
            ("C", "CREATE"),
            ("Q", "QUIT"),
        ];
        t.status_bar_line(left, right)
    };

    f.render_widget(Paragraph::new(status_line), root[0]);

    let main = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
        .split(root[1]);

    let items: Vec<ListItem> = app
        .projects
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let active = app.list_state.selected() == Some(i);

            let dot = if p.is_dirty { t.dot_warn() } else { t.dot_on() };

            let (prefix, name_style) = if active {
                (Span::styled("❯ ", t.accent()), t.accent())
            } else {
                (Span::styled("  ", t.text_disabled()), t.text_dim())
            };

            let sync_sym = if p.ahead > 0 && p.behind > 0 {
                " ⇅"
            } else if p.ahead > 0 {
                " ↑"
            } else if p.behind > 0 {
                " ↓"
            } else {
                ""
            };

            let gpg_indicator = if p.gpg_status.is_configured {
                Span::styled(" ✓", t.success())
            } else if p.gpg_status.signing_key.is_some() {
                Span::styled(" ⚠", t.warning())
            } else {
                Span::styled(" ○", t.text_disabled())
            };

            let remote_indicator = match p.remote_status {
                Some(true) => Span::styled(" ●", t.success()),
                Some(false) => Span::styled(" ◌", t.danger()),
                None => Span::styled(" -", t.text_disabled()),
            };

            let line = Line::from(vec![
                prefix,
                dot,
                Span::raw(" "),
                Span::styled(&p.name, name_style),
                Span::styled(sync_sym, t.accent_light()),
                gpg_indicator,
                remote_indicator,
            ]);
            ListItem::new(line)
        })
        .collect();

    let list_block = Block::default()
        .title(Span::styled(" PROJECTS ", t.block_title()))
        .borders(Borders::ALL)
        .border_style(t.border());

    let list = List::new(items)
        .block(list_block)
        .highlight_style(t.row_selected());

    f.render_stateful_widget(list, main[0], &mut app.list_state);

    let right = main[1];

    if let InputMode::CreatingProject = app.input_mode {
        let cursor = t.cursor_span(app.tick);
        let mut prompt_spans = vec![
            Span::styled("  NEW PROJECT  ❯  ", t.accent()),
            Span::styled(&app.input_buffer, t.text()),
        ];
        prompt_spans.push(cursor);

        let input_para = Paragraph::new(Line::from(prompt_spans)).block(
            Block::default()
                .title(Span::styled(
                    " [ ENTER ] CONFIRM  [ ESC ] CANCEL ",
                    t.input_prompt(),
                ))
                .borders(Borders::ALL)
                .border_style(t.border_active()),
        );
        f.render_widget(input_para, right);
        return;
    }

    if let InputMode::ConfigGpgKey = app.input_mode {
        let cursor = t.cursor_span(app.tick);
        let mut prompt_spans = vec![
            Span::styled("  GPG SIGNING KEY  ❯  ", t.accent()),
            Span::styled(&app.input_buffer, t.text()),
        ];
        prompt_spans.push(cursor);

        let input_para = Paragraph::new(Line::from(prompt_spans)).block(
            Block::default()
                .title(Span::styled(
                    " [ ENTER ] APPLY TO ALL  [ ESC ] CANCEL ",
                    t.input_prompt(),
                ))
                .borders(Borders::ALL)
                .border_style(t.border_active()),
        );
        f.render_widget(input_para, right);
        
        if let Some(msg) = &app.status_message {
            let hint = Paragraph::new(Span::styled(msg, t.text_dim())).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(t.border())
            );
            f.render_widget(hint, Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(3)]).split(root[1])[1]);
        }
        return;
    }

    let Some(idx) = app.list_state.selected() else {
        let msg = if app.is_loading {
            "  Scanning repositories…"
        } else if app.projects.is_empty() {
            "  No projects found. Press [R] to refresh."
        } else {
            "  Select a project."
        };
        let placeholder = Paragraph::new(Span::styled(msg, t.text_disabled())).block(
            Block::default()
                .title(Span::styled(" DETAILS ", t.block_title()))
                .borders(Borders::ALL)
                .border_style(t.border()),
        );
        f.render_widget(placeholder, right);
        return;
    };

    let Some(p) = app.projects.get(idx) else {
        return;
    };

    let mut lines: Vec<Line> = Vec::new();

    let project_title = p.name.to_uppercase();
    lines.push(t.panel_header_line(&project_title));
    lines.push(Line::from(""));

    lines.push(Line::from(vec![
        Span::styled(TREE_BRANCH, t.text_muted()),
        Span::styled("PATH    ", t.label()),
        Span::styled(p.path.to_string_lossy().to_string(), t.text_muted()),
    ]));

    let mut branch_spans = vec![
        Span::styled(TREE_BRANCH, t.text_muted()),
        Span::styled("BRANCH  ", t.label()),
        Span::styled(&p.current_branch, t.branch_name()),
    ];
    if p.ahead > 0 || p.behind > 0 {
        branch_spans.push(Span::styled("  ", t.text_muted()));
        if p.ahead > 0 {
            branch_spans.push(Span::styled(
                format!("↑{} ", p.ahead),
                t.git_ahead().add_modifier(Modifier::BOLD),
            ));
        }
        if p.behind > 0 {
            branch_spans.push(Span::styled(
                format!("↓{} ", p.behind),
                t.git_behind().add_modifier(Modifier::BOLD),
            ));
        }
    } else {
        branch_spans.push(Span::styled("  synced", t.text_disabled()));
    }
    lines.push(Line::from(branch_spans));

    let mut status_spans = vec![
        Span::styled(TREE_LAST, t.text_muted()),
        Span::styled("STATUS  ", t.label()),
        if p.is_dirty {
            Span::styled("DIRTY", t.git_dirty().add_modifier(Modifier::BOLD))
        } else {
            Span::styled("CLEAN", t.git_clean().add_modifier(Modifier::BOLD))
        },
    ];
    if p.is_dirty {
        status_spans.push(Span::styled("  [", t.text_muted()));
        if p.modified > 0 {
            status_spans.push(Span::styled(format!("~{} ", p.modified), t.git_modified()));
        }
        if p.untracked > 0 {
            status_spans.push(Span::styled(
                format!("+{} ", p.untracked),
                t.git_untracked(),
            ));
        }
        if p.deleted > 0 {
            status_spans.push(Span::styled(format!("-{} ", p.deleted), t.git_deleted()));
        }
        status_spans.push(Span::styled("]", t.text_muted()));
    }
    lines.push(Line::from(status_spans));

    lines.push(Line::from(""));

    lines.push(t.panel_header_line("GPG SIGNING"));
    let gpg_line = if p.gpg_status.is_configured {
        vec![
            Span::styled("  ", t.text_muted()),
            Span::styled("✓ CONFIGURED", t.success().add_modifier(Modifier::BOLD)),
            Span::styled("  ", t.text_muted()),
            Span::styled(p.gpg_status.signing_key.as_deref().unwrap_or(""), t.text_dim()),
        ]
    } else if p.gpg_status.signing_key.is_some() {
        vec![
            Span::styled("  ", t.text_muted()),
            Span::styled("⚠ PARTIAL", t.warning().add_modifier(Modifier::BOLD)),
            Span::styled("  (gpgsign not enabled)", t.text_disabled()),
        ]
    } else {
        vec![
            Span::styled("  ", t.text_muted()),
            Span::styled("○ NOT CONFIGURED", t.text_disabled()),
        ]
    };
    lines.push(Line::from(gpg_line));

    if p.gpg_status.auto_retrieve {
        lines.push(Line::from(vec![
            Span::styled("  ", t.text_muted()),
            Span::styled("✓ autoKeyRetrieve enabled", t.text_dim()),
        ]));
    }

    lines.push(Line::from(""));

    lines.push(t.panel_header_line("REMOTE CONNECTION"));
    let remote_line = match p.remote_status {
        Some(true) => vec![
            Span::styled("  ", t.text_muted()),
            Span::styled("● CONNECTED", t.success().add_modifier(Modifier::BOLD)),
        ],
        Some(false) => vec![
            Span::styled("  ", t.text_muted()),
            Span::styled("◌ FAILED", t.danger().add_modifier(Modifier::BOLD)),
        ],
        None => vec![
            Span::styled("  ", t.text_muted()),
            Span::styled("- NO REMOTE", t.text_disabled()),
        ],
    };
    lines.push(Line::from(remote_line));

    lines.push(Line::from(""));

    lines.push(t.panel_header_line("LAST COMMIT"));
    lines.push(Line::from(vec![
        Span::styled("  ", t.text_muted()),
        Span::styled(format_time_ago(p.last_commit_time), t.text_dim()),
    ]));
    lines.push(Line::from(Span::styled(
        format!(
            "  {}",
            p.last_commit_msg.as_deref().unwrap_or("no commits").trim()
        ),
        t.commit_msg(),
    )));

    if let Some(gh) = &p.github_stats {
        lines.push(Line::from(""));
        lines.push(t.panel_header_line("GITHUB"));
        lines.push(Line::from(vec![
            Span::styled("  ★ ", t.github_stats()),
            Span::styled(
                gh.stars.to_string(),
                t.github_stats().add_modifier(Modifier::BOLD),
            ),
            Span::styled("  ⑂ ", t.text_muted()),
            Span::styled(gh.forks.to_string(), t.text_dim()),
            Span::styled("  ⚐ ", t.text_muted()),
            Span::styled(gh.open_issues.to_string(), t.text_dim()),
        ]));
        lines.push(Line::from(vec![
            Span::styled("  ", t.text_muted()),
            Span::styled(&gh.url, t.link()),
        ]));
    }

    if p.is_dirty {
        lines.push(Line::from(""));
        lines.push(t.panel_header_line("CHANGES"));

        let total = p.modified + p.untracked + p.deleted;
        let shown = p.changed_files.len();

        for (i, (file_path, status)) in p.changed_files.iter().enumerate() {
            let (sym, style) = match status {
                FileStatusType::Modified => ("~", t.git_modified()),
                FileStatusType::Untracked => ("+", t.git_untracked()),
                FileStatusType::Deleted => ("-", t.git_deleted()),
            };
            let is_last = i == shown - 1 && total <= shown;
            let prefix = if is_last { TREE_LAST } else { TREE_BRANCH };
            lines.push(Line::from(vec![
                Span::styled(prefix, t.text_muted()),
                Span::styled(format!("{} ", sym), style.add_modifier(Modifier::BOLD)),
                Span::styled(file_path, t.text_dim()),
            ]));
        }

        if total > shown {
            lines.push(Line::from(vec![
                Span::styled(TREE_LAST, t.text_muted()),
                Span::styled(format!("… {} more", total - shown), t.text_disabled()),
            ]));
        }
    } else {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("  ", t.text_muted()),
            Span::styled(
                "✓ workspace clean",
                t.git_clean().add_modifier(Modifier::BOLD),
            ),
        ]));
    }

    let detail = Paragraph::new(lines)
        .block(
            Block::default()
                .title(Span::styled(" DETAILS ", t.block_title()))
                .borders(Borders::ALL)
                .border_style(t.border_active()),
        )
        .wrap(Wrap { trim: true });

    f.render_widget(detail, right);
}