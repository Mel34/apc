use alpm::{Alpm, AnyDownloadEvent, DownloadEvent, DownloadResult, Question, SigLevel, TransFlag, Usage};
use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Clear, List, ListItem, Paragraph, Wrap,
    },
    Frame, Terminal,
};
use std::{
    cell::Cell,
    collections::HashSet,
    fs,
    io::{self, Write},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::Duration,
};

// --- Classic DOS VGA Palette ---
const NC_BLUE: Color = Color::Rgb(0, 0, 170);       // #0000AA
const NC_CYAN: Color = Color::Rgb(0, 170, 170);     // #00AAAA
const NC_WHITE: Color = Color::Rgb(255, 255, 255);  // #FFFFFF
const NC_YELLOW: Color = Color::Rgb(255, 255, 85);  // #FFFF55
const NC_GRAY: Color = Color::Rgb(170, 170, 170);   // #AAAAAA
const NC_DARK_GRAY: Color = Color::Rgb(85, 85, 85); // #555555
const NC_BLACK: Color = Color::Rgb(0, 0, 0);

const IGNORE_PKG_FILE: &str = "/etc/pacman.d/IgnorePkg";

static LOG_TO_STDOUT: AtomicBool = AtomicBool::new(false);

thread_local! {
    static ACTIVE_TERM: Cell<*mut Terminal<CrosstermBackend<std::fs::File>>> = Cell::new(std::ptr::null_mut());
    static BACKGROUND_BUFFER: std::cell::RefCell<Option<ratatui::buffer::Buffer>> = const { std::cell::RefCell::new(None) };
}

#[derive(Clone)]
struct PkgSummary {
    name: String,
    version: String,
    #[allow(dead_code)]
    desc: String,
    size: i64,
}

#[derive(Clone)]
struct UpdateItem {
    name: String,
    new_version: String,
    size: i64,
}

struct SysUpdateInfo {
    updates: Vec<UpdateItem>,
    new_deps: Vec<UpdateItem>,
    total_download_size: i64,
}

#[derive(Clone)]
struct CachedVersionItem {
    path: std::path::PathBuf,
    version: String,
    size: i64,
    build_date: i64,
}

struct DowngradeView {
    #[allow(dead_code)]
        pkg_name: String,
    installed_version: String,
    versions: Vec<CachedVersionItem>,
    cursor: usize,
}

#[derive(PartialEq, Clone, Copy)]
enum ActivePane {
    Left,  // Installed
    Right, // Available (Repos)
}

struct PaneState {
    all_items: Vec<PkgSummary>,
    cursor: usize,
    filter: String,
    tagged: HashSet<String>,
}

impl PaneState {
    fn new(items: Vec<PkgSummary>) -> Self {
        Self {
            all_items: items,
            cursor: 0,
            filter: String::new(),
            tagged: HashSet::new(),
        }
    }

    fn filtered(&self) -> Vec<&PkgSummary> {
        if self.filter.is_empty() {
            self.all_items.iter().collect()
        } else {
            let f = self.filter.to_lowercase();
            self.all_items
                .iter()
                .filter(|p| p.name.to_lowercase().contains(&f) || self.tagged.contains(&p.name))
                .collect()
        }
    }

    fn current_pkg(&self) -> Option<&PkgSummary> {
        let f = self.filtered();
        f.get(self.cursor).copied()
    }

    fn move_up(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
        }
    }

    fn move_down(&mut self) {
        let count = self.filtered().len();
        if count > 0 && self.cursor < count - 1 {
            self.cursor += 1;
        }
    }

    fn page_up(&mut self, amount: usize) {
        self.cursor = self.cursor.saturating_sub(amount);
    }

    fn page_down(&mut self, amount: usize) {
        let count = self.filtered().len();
        if count > 0 {
            self.cursor = (self.cursor + amount).min(count - 1);
        }
    }

    fn toggle_tag(&mut self) {
        if let Some(pkg) = self.current_pkg() {
            let name = pkg.name.clone();
            if self.tagged.contains(&name) {
                self.tagged.remove(&name);
            } else {
                self.tagged.insert(name);
            }
            self.move_down();
        }
    }
}

enum DialogAction {
    Delete,
    Install(Vec<String>),
    Reinstall(Vec<String>),
    Downgrade {
        #[allow(dead_code)]
        pkg_name: String,
        target_version: String,
        path: std::path::PathBuf,
    },
}

struct ConfirmationDialog {
    title: String,
    packages: Vec<String>,
    additional_removals: Vec<String>,
    hold_packages: Vec<String>,
    total_size: i64,
    selected_button: usize,
    button_labels: Vec<String>,
    action: DialogAction,
}

#[derive(Clone)]
struct ProgressState {
    title: String,
    target: String,
    step: String,
    percent: u16,
}

#[derive(Clone)]
struct PackageProgressItem {
    name: String,
    percent: u16,
}

#[derive(Clone)]
struct DownloadProgressItem {
    name: String,
    downloaded: i64,
    total: i64,
    percent: u16,
}

#[derive(Clone, Default)]
struct PackageProgressState {
    packages: Vec<PackageProgressItem>,
    active_package: Option<String>,
    overall_percent: u16,
    downloads: Vec<DownloadProgressItem>,
    active_download: Option<String>,
    download_percent: u16,
    downloaded_bytes: i64,
    total_download_bytes: i64,
}

static PACKAGE_PROGRESS: std::sync::Mutex<PackageProgressState> =
    std::sync::Mutex::new(PackageProgressState {
        packages: Vec::new(),
        active_package: None,
        overall_percent: 0,
        downloads: Vec::new(),
        active_download: None,
        download_percent: 0,
        downloaded_bytes: 0,
        total_download_bytes: 0,
    });

static QUESTION_REFUSALS: std::sync::Mutex<Vec<String>> =
    std::sync::Mutex::new(Vec::new());

thread_local! {
    static ACTIVE_PROGRESS: std::cell::RefCell<Option<ProgressState>> =
        const { std::cell::RefCell::new(None) };
}

fn clear_question_refusals() {
    QUESTION_REFUSALS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
}

fn take_question_refusal_details() -> Option<String> {
    let refusals = std::mem::take(
        &mut *QUESTION_REFUSALS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
    );
    if refusals.is_empty() {
        None
    } else {
        Some(format!(
            "libalpm declined these questions using the safe default (No):\n{}",
            refusals
                .iter()
                .map(|question| format!("  • {question}"))
                .collect::<Vec<_>>()
                .join("\n")
        ))
    }
}

fn with_question_refusals(message: String) -> String {
    match take_question_refusal_details() {
        Some(details) => format!("{message}\n{details}"),
        None => message,
    }
}

fn redraw_transaction_progress() {
    let background = BACKGROUND_BUFFER.with(|buffer| buffer.borrow().clone());
    let Some(background) = background else {
        return;
    };
    let progress = ACTIVE_PROGRESS.with(|progress| progress.borrow().clone());
    let state = PACKAGE_PROGRESS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();

    ACTIVE_TERM.with(|term| {
        let terminal_ptr = term.get();
        if terminal_ptr.is_null() {
            return;
        }

        unsafe {
            let _ = (*terminal_ptr).draw(|frame| {
                *frame.buffer_mut() = background.clone();
                if let Some(progress) = progress.as_ref() {
                    draw_progress_dialog(frame, progress, &state);
                }
            });
        }
    });
}

enum Modal {
    Confirm(ConfirmationDialog),
    Help,
    Message(String),
}

fn parse_siglevel(tokens: &[String], mut level: SigLevel) -> Result<SigLevel> {
    fn set_requirement(level: &mut SigLevel, package: bool, requirement: &str) {
        let (required, optional) = if package {
            (SigLevel::PACKAGE, SigLevel::PACKAGE_OPTIONAL)
        } else {
            (SigLevel::DATABASE, SigLevel::DATABASE_OPTIONAL)
        };
        match requirement {
            "required" => { level.insert(required); level.remove(optional); }
            "optional" => { level.insert(required | optional); }
            "never" => { level.remove(required | optional); }
            _ => {}
        }
    }
    for token in tokens.iter().flat_map(|value| value.split_whitespace()) {
        let value = token.to_ascii_lowercase();
        match value.as_str() {
            "required" | "optional" | "never" => {
                set_requirement(&mut level, true, &value);
                set_requirement(&mut level, false, &value);
            }
            "packagerequired" => set_requirement(&mut level, true, "required"),
            "packageoptional" => set_requirement(&mut level, true, "optional"),
            "packagenever" => set_requirement(&mut level, true, "never"),
            "databaserequired" => set_requirement(&mut level, false, "required"),
            "databaseoptional" => set_requirement(&mut level, false, "optional"),
            "databasenever" => set_requirement(&mut level, false, "never"),
            "trustedonly" => level.remove(
                SigLevel::PACKAGE_MARGINAL_OK | SigLevel::PACKAGE_UNKNOWN_OK |
                SigLevel::DATABASE_MARGINAL_OK | SigLevel::DATABASE_UNKNOWN_OK
            ),
            "packagetrustedonly" => level.remove(SigLevel::PACKAGE_MARGINAL_OK | SigLevel::PACKAGE_UNKNOWN_OK),
            "databasetrustedonly" => level.remove(SigLevel::DATABASE_MARGINAL_OK | SigLevel::DATABASE_UNKNOWN_OK),
            "trustall" => level.insert(
                SigLevel::PACKAGE_MARGINAL_OK | SigLevel::PACKAGE_UNKNOWN_OK |
                SigLevel::DATABASE_MARGINAL_OK | SigLevel::DATABASE_UNKNOWN_OK
            ),
            "packagetrustall" => level.insert(SigLevel::PACKAGE_MARGINAL_OK | SigLevel::PACKAGE_UNKNOWN_OK),
            "databasetrustall" => level.insert(SigLevel::DATABASE_MARGINAL_OK | SigLevel::DATABASE_UNKNOWN_OK),
            "packagemarginalok" => level.insert(SigLevel::PACKAGE_MARGINAL_OK),
            "packageunknownok" => level.insert(SigLevel::PACKAGE_UNKNOWN_OK),
            "databasemarginalok" => level.insert(SigLevel::DATABASE_MARGINAL_OK),
            "databaseunknownok" => level.insert(SigLevel::DATABASE_UNKNOWN_OK),
            _ => anyhow::bail!("Unsupported SigLevel option in pacman.conf: {token}"),
        }
    }
    Ok(level)
}

fn parse_repo_usage(tokens: &[String]) -> Result<Usage> {
    if tokens.is_empty() || tokens.iter().any(|v| v.eq_ignore_ascii_case("all")) {
        return Ok(Usage::ALL);
    }
    let mut usage = Usage::NONE;
    for token in tokens.iter().flat_map(|value| value.split_whitespace()) {
        match token.to_ascii_lowercase().as_str() {
            "sync" => usage.insert(Usage::SYNC),
            "search" => usage.insert(Usage::SEARCH),
            "install" => usage.insert(Usage::INSTALL),
            "upgrade" => usage.insert(Usage::UPGRADE),
            _ => anyhow::bail!("Unsupported repository Usage option: {token}"),
        }
    }
    Ok(usage)
}

fn wildcard_matches(pattern: &str, value: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let value: Vec<char> = value.chars().collect();
    let mut matched = vec![false; value.len() + 1];
    matched[0] = true;
    for ch in pattern {
        if ch == '*' {
            for index in 1..=value.len() { matched[index] = matched[index] || matched[index - 1]; }
        } else {
            for index in (1..=value.len()).rev() {
                matched[index] = matched[index - 1] && (ch == '?' || ch == value[index - 1]);
            }
            matched[0] = false;
        }
    }
    matched[value.len()]
}

fn is_hold_package(name: &str, hold_pkgs: &[String]) -> bool {
    hold_pkgs.iter().any(|pattern| wildcard_matches(pattern, name))
}

fn package_matches_patterns(name: &str, patterns: &HashSet<String>) -> bool {
    patterns.iter().any(|pattern| wildcard_matches(pattern, name))
}

fn is_managed_wildcard_lock(name: &str, patterns: &HashSet<String>) -> bool {
    patterns.iter().any(|pattern| {
        (pattern.contains('*') || pattern.contains('?'))
            && wildcard_matches(pattern, name)
    })
}

fn managed_wildcard_patterns_to_remove(
    targets: &[String],
    installed: &[PkgSummary],
    patterns: &HashSet<String>,
) -> Result<HashSet<String>> {
    let selected: HashSet<&str> = targets.iter().map(String::as_str).collect();
    let mut to_remove = HashSet::new();

    for pattern in patterns
        .iter()
        .filter(|pattern| pattern.contains('*') || pattern.contains('?'))
    {
        let matches: HashSet<&str> = installed
            .iter()
            .filter(|pkg| wildcard_matches(pattern, &pkg.name))
            .map(|pkg| pkg.name.as_str())
            .collect();

        if matches.iter().any(|name| selected.contains(name)) {
            let missing: Vec<&str> = matches.difference(&selected).copied().collect();
            if !missing.is_empty() {
                anyhow::bail!(
                    "Cannot unlock wildcard '{pattern}': select all matching installed packages ({})",
                    missing.join(", ")
                );
            }
            to_remove.insert(pattern.clone());
        }
    }

    if !to_remove.is_empty() {
        let uncovered: Vec<&str> = targets
            .iter()
            .map(String::as_str)
            .filter(|name| {
                !to_remove
                    .iter()
                    .any(|pattern| wildcard_matches(pattern, name))
            })
            .collect();
        if !uncovered.is_empty() {
            anyhow::bail!(
                "Cannot mix wildcard unlocks with other package locks; selection also contains {}",
                uncovered.join(", ")
            );
        }
    }

    Ok(to_remove)
}

fn parse_package_names(values: &[String]) -> Vec<String> {
    values
        .iter()
        .flat_map(|value| value.split_whitespace().map(str::to_string))
        .collect()
}

fn expand_include_pattern(pattern: &str, config_path: &Path) -> Vec<std::path::PathBuf> {
    let include_path = Path::new(pattern);
    let include_path = if include_path.is_absolute() {
        include_path.to_path_buf()
    } else {
        config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(include_path)
    };

    let mut candidates = Vec::new();
    for component in include_path.components() {
        match component {
            std::path::Component::RootDir => candidates = vec![std::path::PathBuf::from("/")],
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                for candidate in &mut candidates {
                    candidate.push("..");
                }
            }
            std::path::Component::Normal(name) => {
                let name = name.to_string_lossy();
                if name.contains('*') || name.contains('?') {
                    let mut matches = Vec::new();
                    for parent in &candidates {
                        if let Ok(entries) = fs::read_dir(parent) {
                            for entry in entries.flatten() {
                                if wildcard_matches(&name, &entry.file_name().to_string_lossy()) {
                                    matches.push(entry.path());
                                }
                            }
                        }
                    }
                    candidates = matches;
                } else {
                    for candidate in &mut candidates {
                        candidate.push(name.to_string());
                    }
                }
            }
            std::path::Component::Prefix(_) => {}
        }
    }
    candidates
}

fn config_includes_file(
    config_path: &Path,
    target_path: &Path,
    visited: &mut HashSet<std::path::PathBuf>,
) -> bool {
    let config_path = fs::canonicalize(config_path).unwrap_or_else(|_| config_path.to_path_buf());
    if !visited.insert(config_path.clone()) {
        return false;
    }

    let Ok(content) = fs::read_to_string(&config_path) else {
        return false;
    };

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };
        if !key.trim().eq_ignore_ascii_case("Include") {
            continue;
        }

        let pattern = value
            .split('#')
            .next()
            .unwrap_or("")
            .trim()
            .trim_matches('"')
            .trim_matches('\'');
        for included_path in expand_include_pattern(pattern, &config_path) {
            let resolved = fs::canonicalize(&included_path)
                .unwrap_or_else(|_| included_path.clone());
            if resolved == target_path {
                return true;
            }
            if included_path.is_file()
                && config_includes_file(&included_path, target_path, visited)
            {
                return true;
            }
        }
    }
    false
}

fn pacman_config_includes_managed_ignore_file() -> bool {
    let config_path = Path::new("/etc/pacman.conf");
    let target_path = Path::new(IGNORE_PKG_FILE);
    let Ok(target_path) = fs::canonicalize(target_path) else {
        return false;
    };
    config_includes_file(config_path, &target_path, &mut HashSet::new())
}

fn parse_external_ignore_packages(
    configured: &[String],
    managed: &HashSet<String>,
    managed_file_included: bool,
) -> HashSet<String> {
    // pacmanconf expands Include directives and loses their source-file provenance.
    // Only subtract APC-managed entries when the managed file is in the Include chain.
    let mut packages = parse_package_names(configured);
    if managed_file_included {
        for managed_pkg in managed {
            if let Some(index) = packages.iter().position(|pkg| pkg == managed_pkg) {
                packages.remove(index);
            }
        }
    }
    packages.into_iter().collect()
}

fn parse_managed_ignore_pkg(content: &str) -> HashSet<String> {
    let mut packages = HashSet::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = trimmed.split_once('=') {
            if key.trim().eq_ignore_ascii_case("IgnorePkg") {
                packages.extend(value.split_whitespace().map(str::to_string));
            }
        }
    }
    packages
}

struct TerminalGuard {
    tty: std::fs::File,
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(&mut self.tty, LeaveAlternateScreen);
    }
}

struct CommitGuard {
    saved_stdout: libc::c_int,
    saved_stderr: libc::c_int,
}

impl Drop for CommitGuard {
    fn drop(&mut self) {
        ACTIVE_TERM.with(|term| term.set(std::ptr::null_mut()));
        ACTIVE_PROGRESS.with(|progress| *progress.borrow_mut() = None);
        BACKGROUND_BUFFER.with(|buffer| *buffer.borrow_mut() = None);
        let mut state = PACKAGE_PROGRESS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        *state = PackageProgressState::default();
        if self.saved_stdout >= 0 {
            unsafe { libc::dup2(self.saved_stdout, 1); libc::close(self.saved_stdout); }
        }
        if self.saved_stderr >= 0 {
            unsafe { libc::dup2(self.saved_stderr, 2); libc::close(self.saved_stderr); }
        }
    }
}

struct App {
    alpm: Alpm,
    active_pane: ActivePane,
    left: PaneState,
    right: PaneState,
    info_mode: bool,
    locked_pkgs: HashSet<String>,
    pacman_conf_ignored_pkgs: HashSet<String>,
    updates_view: Option<SysUpdateInfo>,
    upgrade_selected_button: usize,
    downgrade_view: Option<DowngradeView>,
    cache_dirs: Vec<String>,
    modal: Option<Modal>,
    progress: Option<ProgressState>,
    system_upgrade_in_progress: bool,
    syncdbs_synced_without_upgrade: bool,
    partial_upgrade_marker: std::path::PathBuf,
    hold_pkgs: Vec<String>,
    should_quit: bool,
}

impl App {
    fn mark_partial_upgrade_pending(&mut self) -> Result<()> {
        if let Err(err) = fs::write(&self.partial_upgrade_marker, b"APC requires a full system upgrade after database synchronization.\n") {
            self.progress = None;
            return Err(err.into());
        }
        self.syncdbs_synced_without_upgrade = true;
        Ok(())
    }

    fn clear_partial_upgrade_pending(&mut self) -> Result<()> {
        match fs::remove_file(&self.partial_upgrade_marker) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                self.syncdbs_synced_without_upgrade = true;
                return Err(err.into());
            }
        }
        self.syncdbs_synced_without_upgrade = false;
        Ok(())
    }

    fn new() -> Result<Self> {
        let pacconf = pacmanconf::Config::new()?;
        let mut alpm = Alpm::new(pacconf.root_dir.as_str(), pacconf.db_path.as_str())?;
        let partial_upgrade_marker = Path::new(alpm.lockfile())
            .parent()
            .unwrap_or_else(|| Path::new("/var/lib/pacman"))
            .join("apc-partial-upgrade-pending");
        let syncdbs_synced_without_upgrade = partial_upgrade_marker.exists();

        let global_sig_level = parse_siglevel(
            &pacconf.sig_level,
            SigLevel::PACKAGE | SigLevel::DATABASE,
        )?;
        alpm.set_default_siglevel(global_sig_level)?;
        alpm.set_local_file_siglevel(parse_siglevel(&pacconf.local_file_sig_level, global_sig_level)?)?;
        alpm.set_remote_file_siglevel(parse_siglevel(&pacconf.remote_file_sig_level, global_sig_level)?)?;
        alpm.set_gpgdir(pacconf.gpg_dir.as_str())?;
        alpm.set_logfile(pacconf.log_file.as_str())?;
        alpm.set_check_space(pacconf.check_space);
        alpm.set_use_syslog(pacconf.use_syslog);
        alpm.set_disable_dl_timeout(pacconf.disable_download_timeout);
        alpm.set_parallel_downloads(pacconf.parallel_downloads.min(u32::MAX as u64) as u32);
        alpm.set_disable_sandbox_filesystem(pacconf.disable_sandbox || pacconf.disable_sandbox_filesystem);
        alpm.set_disable_sandbox_syscalls(pacconf.disable_sandbox || pacconf.disable_sandbox_syscalls);
        if let Some(download_user) = pacconf.download_user.as_deref() {
            alpm.set_sandbox_user(Some(download_user))?;
        }

        let mut cache_dirs = Vec::new();
        for cachedir in &pacconf.cache_dir {
            let normalized = cachedir.trim_end_matches('/');
            if !cache_dirs.iter().any(|existing: &String| existing.trim_end_matches('/') == normalized) {
                alpm.add_cachedir(cachedir.as_str())?;
                cache_dirs.push(cachedir.clone());
            }
        }
        for hook_dir in &pacconf.hook_dir { alpm.add_hookdir(hook_dir.as_str())?; }
        for value in &pacconf.no_upgrade {
            for pattern in value.split_whitespace() { alpm.add_noupgrade(pattern)?; }
        }
        for value in &pacconf.no_extract {
            for pattern in value.split_whitespace() { alpm.add_noextract(pattern)?; }
        }
        for value in &pacconf.ignore_group {
            for group in value.split_whitespace() { alpm.add_ignoregroup(group)?; }
        }

        let configured_architectures: Vec<String> = pacconf.architecture.iter()
            .flat_map(|value| value.split_whitespace().map(str::to_string))
            .collect();
        let architectures: Vec<String> = if configured_architectures.is_empty()
            || configured_architectures.iter().any(|arch| arch.eq_ignore_ascii_case("auto"))
        {
            vec![std::env::consts::ARCH.to_string()]
        } else {
            configured_architectures
        };
        for architecture in &architectures {
            alpm.add_architecture(architecture.as_str())?;
        }
        let arch = architectures.first().cloned().unwrap_or_else(|| std::env::consts::ARCH.to_string());

        for repo in &pacconf.repos {
            for server in &repo.servers {
                let expanded = server.replace("$repo", &repo.name).replace("$arch", &arch);
                if let Some(local_path) = expanded.strip_prefix("file://") {
                    let clean_path = local_path.trim_end_matches('/');
                    let already_configured = cache_dirs.iter()
                        .any(|existing| existing.trim_end_matches('/') == clean_path);
                    if !already_configured && Path::new(clean_path).is_dir() {
                        alpm.add_cachedir(clean_path)?;
                        cache_dirs.push(clean_path.to_string());
                    }
                }
            }
        }
        for repo in &pacconf.repos {
            let repo_sig_level = parse_siglevel(&repo.sig_level, global_sig_level)?;
            let db = alpm.register_syncdb_mut(repo.name.as_str(), repo_sig_level)?;
            db.set_usage(parse_repo_usage(&repo.usage)?)?;
            for server in &repo.servers {
                let expanded = server.replace("$repo", &repo.name).replace("$arch", &arch);
                db.add_server(expanded.as_str())?;
            }
        }

        let mut locked_pkgs = Self::load_locked_packages();
        let pacman_conf_ignored_pkgs = parse_external_ignore_packages(
            &pacconf.ignore_pkg,
            &locked_pkgs,
            pacman_config_includes_managed_ignore_file(),
        );
        // If a package is also excluded outside APC's managed file, external
        // configuration takes ownership and APC must not offer to unlock it.
        locked_pkgs.retain(|pkg| {
            !pacman_conf_ignored_pkgs
                .iter()
                .any(|pattern| wildcard_matches(pattern, pkg))
        });
        for pkg in locked_pkgs.union(&pacman_conf_ignored_pkgs) {
            alpm.add_ignorepkg(pkg.as_str())?;
        }
        let hold_pkgs: Vec<String> = pacconf.hold_pkg.iter()
            .flat_map(|value| value.split_whitespace().map(str::to_string))
            .collect();

        // Live In-TUI progress callback: record package progress and redraw the current dialog
        alpm.set_progress_cb((), move |progress, pkgname, percent, howmany, current, _| {
            let overall = if howmany > 0 {
                ((current.saturating_sub(1) * 100 + percent.max(0) as usize) / howmany).min(100) as u16
            } else {
                percent.max(0).min(100) as u16
            };

            if matches!(
                progress,
                alpm::Progress::AddStart
                    | alpm::Progress::UpgradeStart
                    | alpm::Progress::DowngradeStart
                    | alpm::Progress::ReinstallStart
                    | alpm::Progress::RemoveStart
            ) {
                let mut state = PACKAGE_PROGRESS
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());

                if state.active_package.as_deref() != Some(pkgname) {
                    if let Some(previous) = state.active_package.clone() {
                        if let Some(item) = state.packages.iter_mut().find(|item| item.name == previous) {
                            item.percent = 100;
                        }
                    }

                    if !state.packages.iter().any(|item| item.name == pkgname) {
                        state.packages.push(PackageProgressItem {
                            name: pkgname.to_string(),
                            percent: 0,
                        });
                    }
                    state.active_package = Some(pkgname.to_string());
                }

                if let Some(item) = state.packages.iter_mut().find(|item| item.name == pkgname) {
                    item.percent = percent.max(0).min(100) as u16;
                }
                state.overall_percent = overall;
            }

            redraw_transaction_progress();
        });

        // Refuse unresolved ALPM questions rather than silently approving package
        // replacement, conflict removal, ignored-package installation, or key import.
        // The refusal is attached to the transaction error so the user sees why it stopped.
        alpm.set_question_cb((), |mut question, _| {
            let details = format!("{question:?}");
            match question.question() {
                Question::InstallIgnorepkg(mut prompt) => prompt.set_install(false),
                Question::Replace(prompt) => prompt.set_replace(false),
                Question::Conflict(mut prompt) => prompt.set_remove(false),
                Question::Corrupted(mut prompt) => prompt.set_remove(false),
                Question::RemovePkgs(mut prompt) => prompt.set_skip(true),
                Question::SelectProvider(mut prompt) => prompt.set_index(-1),
                Question::ImportKey(mut prompt) => prompt.set_import(false),
            }
            QUESTION_REFUSALS
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(details);
        });

        // Report real package-download progress alongside package-operation progress.
        alpm.set_dl_cb((), |filename, event: AnyDownloadEvent<'_>, _| {
            let mut state = PACKAGE_PROGRESS
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());

            match event.event() {
                DownloadEvent::Init(_) => {
                    if !state.downloads.iter().any(|item| item.name == filename) {
                        state.downloads.push(DownloadProgressItem {
                            name: filename.to_string(),
                            downloaded: 0,
                            total: 0,
                            percent: 0,
                        });
                    }
                    state.active_download = Some(filename.to_string());
                }
                DownloadEvent::Progress(progress) => {
                    if let Some(item) = state.downloads.iter_mut().find(|item| item.name == filename) {
                        item.downloaded = progress.downloaded.max(0);
                        item.total = progress.total.max(0);
                        item.percent = if item.total > 0 {
                            ((item.downloaded.saturating_mul(100) / item.total).min(100)) as u16
                        } else {
                            0
                        };
                    }
                    state.active_download = Some(filename.to_string());
                }
                DownloadEvent::Completed(completed) => {
                    if let Some(item) = state.downloads.iter_mut().find(|item| item.name == filename) {
                        item.total = item.total.max(completed.total.max(0));
                        if matches!(completed.result, DownloadResult::Success | DownloadResult::UpToDate) {
                            item.downloaded = item.total;
                            item.percent = 100;
                        }
                    }
                    state.active_download = Some(filename.to_string());
                }
                DownloadEvent::Retry(_) => {
                    state.active_download = Some(filename.to_string());
                }
            }

            state.downloaded_bytes = state.downloads.iter().map(|item| item.downloaded).sum();
            state.total_download_bytes = state.downloads.iter().map(|item| item.total).sum();
            state.download_percent = if state.total_download_bytes > 0 {
                ((state.downloaded_bytes.saturating_mul(100) / state.total_download_bytes).min(100)) as u16
            } else {
                0
            };
            drop(state);
            redraw_transaction_progress();
        });

        // Register ALPM log callback
        alpm.set_log_cb((), |level, msg, _| {
            if LOG_TO_STDOUT.load(Ordering::Relaxed) {
                use alpm::LogLevel;
                if !level.intersects(LogLevel::DEBUG | LogLevel::FUNCTION) || msg.contains("hook") {
                    print!("{msg}");
                    let _ = io::stdout().flush();
                }
            }
        });

        let mut app = Self {
            alpm,
            active_pane: ActivePane::Left,
            left: PaneState::new(Vec::new()),
            right: PaneState::new(Vec::new()),
            info_mode: false,
            locked_pkgs,
            pacman_conf_ignored_pkgs,
            updates_view: None,
            upgrade_selected_button: 0,
            downgrade_view: None,
            cache_dirs,
            modal: None,
            progress: None,
            system_upgrade_in_progress: false,
            syncdbs_synced_without_upgrade,
            partial_upgrade_marker,
            hold_pkgs,
            should_quit: false,
        };

        app.refresh_installed()?;
        app.refresh_available()?;
        Ok(app)
    }

    fn load_locked_packages() -> HashSet<String> {
        fs::read_to_string(IGNORE_PKG_FILE)
            .map(|content| parse_managed_ignore_pkg(&content))
            .unwrap_or_default()
    }

    fn is_package_locked(&self, name: &str) -> bool {
        package_matches_patterns(name, &self.locked_pkgs)
            || package_matches_patterns(name, &self.pacman_conf_ignored_pkgs)
    }

    fn save_locked_packages(&self) -> Result<()> {
        let dir = Path::new("/etc/pacman.d");
        if !dir.exists() {
            let _ = fs::create_dir_all(dir);
        }
        let mut list: Vec<&str> = self
            .locked_pkgs
            .iter()
            .filter(|name| {
                !self
                    .pacman_conf_ignored_pkgs
                    .iter()
                    .any(|pattern| wildcard_matches(pattern, name))
            })
            .map(String::as_str)
            .collect();
        list.sort();

        let content = if list.is_empty() {
            "# Managed by Arch Package Commander (apc)\nIgnorePkg =\n".to_string()
        } else {
            format!(
                "# Managed by Arch Package Commander (apc)\nIgnorePkg = {}\n",
                list.join(" ")
            )
        };

        if let Err(e) = fs::write(IGNORE_PKG_FILE, &content) {
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                let mut child = std::process::Command::new("sudo")
                    .args(["tee", IGNORE_PKG_FILE])
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::null())
                    .spawn()?;
                let mut stdin = child.stdin.take()
                    .ok_or_else(|| anyhow::anyhow!("Failed to open stdin for sudo tee"))?;
                stdin.write_all(content.as_bytes())?;
                drop(stdin);

                let status = child.wait()?;
                if !status.success() {
                    anyhow::bail!("sudo tee failed to save package locks (status: {status})");
                }
            } else {
                return Err(e.into());
            }
        }
        Ok(())
    }

    fn toggle_lock_current(&mut self) -> Result<()> {
        let targets: Vec<String> = if !self.left.tagged.is_empty() {
            self.left.tagged.iter().cloned().collect()
        } else if let Some(pkg) = self.left.current_pkg() {
            vec![pkg.name.clone()]
        } else {
            return Ok(());
        };

        let externally_locked: Vec<&str> = targets
            .iter()
            .filter(|target| {
                self.pacman_conf_ignored_pkgs
                    .iter()
                    .any(|pattern| wildcard_matches(pattern, target))
            })
            .map(String::as_str)
            .collect();
        if !externally_locked.is_empty() {
            anyhow::bail!(
                "Cannot change {}: excluded by pacman.conf; APC leaves those settings untouched",
                externally_locked.join(", ")
            );
        }

        let wildcard_patterns_to_remove = managed_wildcard_patterns_to_remove(
            &targets,
            &self.left.all_items,
            &self.locked_pkgs,
        )?;
        if !wildcard_patterns_to_remove.is_empty() {
            let previous_locked = self.locked_pkgs.clone();
            let mut removed_patterns: Vec<String> = Vec::new();

            for pattern in &wildcard_patterns_to_remove {
                if let Err(err) = self.alpm.remove_ignorepkg(pattern.as_str()).map(|_| ()) {
                    for removed in &removed_patterns {
                        let _ = self.alpm.add_ignorepkg(removed.as_str());
                    }
                    return Err(err.into());
                }
                removed_patterns.push(pattern.clone());
            }

            for pattern in &wildcard_patterns_to_remove {
                self.locked_pkgs.remove(pattern);
            }

            if let Err(err) = self.save_locked_packages() {
                for pattern in &removed_patterns {
                    let _ = self.alpm.add_ignorepkg(pattern.as_str());
                }
                self.locked_pkgs = previous_locked;
                return Err(err);
            }

            self.left.tagged.clear();
            return Ok(());
        }

        let managed_wildcard_locked: Vec<&str> = targets
            .iter()
            .filter(|target| is_managed_wildcard_lock(target, &self.locked_pkgs))
            .map(String::as_str)
            .collect();
        if !managed_wildcard_locked.is_empty() {
            anyhow::bail!(
                "Cannot change {}: covered by an APC-managed wildcard in {}; edit that pattern to change the lock",
                managed_wildcard_locked.join(", "),
                IGNORE_PKG_FILE
            );
        }

        let previous_locked = self.locked_pkgs.clone();
        let any_unlocked = targets.iter().any(|t| !previous_locked.contains(t));

        for t in &targets {
            let result = if any_unlocked {
                self.alpm.add_ignorepkg(t.as_str())
            } else {
                self.alpm.remove_ignorepkg(t.as_str()).map(|_| ())
            };
            if let Err(err) = result {
                for target in &targets {
                    if previous_locked.contains(target) {
                        let _ = self.alpm.add_ignorepkg(target.as_str());
                    } else {
                        let _ = self.alpm.remove_ignorepkg(target.as_str());
                    }
                }
                self.locked_pkgs = previous_locked;
                return Err(err.into());
            }

            if any_unlocked {
                self.locked_pkgs.insert(t.clone());
            } else {
                self.locked_pkgs.remove(t);
            }
        }

        if let Err(err) = self.save_locked_packages() {
            for target in &targets {
                if previous_locked.contains(target) {
                    let _ = self.alpm.add_ignorepkg(target.as_str());
                } else {
                    let _ = self.alpm.remove_ignorepkg(target.as_str());
                }
            }
            self.locked_pkgs = previous_locked;
            return Err(err);
        }

        self.left.tagged.clear();
        Ok(())
    }

    fn prepare_removal(&mut self, targets: &[String]) -> Result<(Vec<String>, Vec<String>, i64)> {
        check_database_lock(&self.alpm)?;
        clear_question_refusals();
        self.alpm.trans_init(TransFlag::RECURSE | TransFlag::UNNEEDED | TransFlag::NO_SAVE)?;
        for name in targets {
            let pkg = match self.alpm.localdb().pkg(name.as_str()) {
                Ok(pkg) => pkg,
                Err(err) => {
                    let message = format!("{err:?}");
                    let _ = self.alpm.trans_release();
                    anyhow::bail!("Selected package '{name}' is not installed: {message}");
                }
            };
            if let Err(err) = self.alpm.trans_remove_pkg(pkg) {
                let message = format!("{err:?}");
                let _ = self.alpm.trans_release();
                anyhow::bail!("Cannot queue '{name}' for removal: {message}");
            }
        }
        let prepare_error = match self.alpm.trans_prepare() {
            Ok(()) => take_question_refusal_details()
                .map(|details| format!("Removal stopped: {details}")),
            Err(err) => {
                let mut details = Vec::new();
                for name in targets {
                    if let Ok(pkg) = self.alpm.localdb().pkg(name.as_str()) {
                        let dependents: Vec<String> = pkg
                            .required_by()
                            .iter()
                            .map(|dependent| dependent.to_string())
                            .collect();
                        if !dependents.is_empty() {
                            details.push(format!("{name} is required by {}", dependents.join(", ")));
                        }
                    }
                }
                let mut message = format!("Safe removal was rejected by dependency checks: {err:?}");
                if !details.is_empty() {
                    message.push_str(&format!("\nInstalled dependents: {}", details.join("; ")));
                }
                Some(with_question_refusals(message))
            }
        };
        if let Some(err) = prepare_error {
            let _ = self.alpm.trans_release();
            anyhow::bail!("{err}");
        }
        let removals: Vec<(String, i64)> = self.alpm.trans_remove().iter()
            .map(|pkg| (pkg.name().to_string(), pkg.isize()))
            .collect();
        let removal_names: HashSet<String> = removals.iter().map(|(name, _)| name.clone()).collect();
        if let Some(missing) = targets.iter().find(|name| !removal_names.contains(*name)) {
            let _ = self.alpm.trans_release();
            anyhow::bail!("libalpm did not include selected package '{missing}' in the prepared removal transaction");
        }
        let mut additional: Vec<String> = removals.iter()
            .map(|(name, _)| name.clone())
            .filter(|name| !targets.contains(name))
            .collect();
        additional.sort();
        let total_size = removals.iter().map(|(_, size)| *size).sum();
        let hold_packages: Vec<String> = removals.iter()
            .map(|(name, _)| name.clone())
            .filter(|name| is_hold_package(name, &self.hold_pkgs))
            .collect();
        Ok((additional, hold_packages, total_size))
    }

    fn open_removal_confirmation(&mut self, targets: Vec<String>) {
        if targets.is_empty() { return; }
        match self.prepare_removal(&targets) {
            Ok((additional_removals, hold_packages, total_size)) => {
                self.modal = Some(Modal::Confirm(ConfirmationDialog {
                    title: "Remove packages (-Rns)".to_string(),
                    packages: targets,
                    additional_removals,
                    hold_packages,
                    total_size,
                    selected_button: 1,
                    button_labels: vec!["Proceed".to_string(), "Cancel".to_string()],
                    action: DialogAction::Delete,
                }));
            }
            Err(err) => self.modal = Some(Modal::Message(format!("Removal blocked: {err}"))),
        }
    }

    fn total_download_size_for_packages(
        &mut self,
        targets: &[String],
        reinstall: bool,
    ) -> Result<i64> {
        if self.syncdbs_synced_without_upgrade {
            anyhow::bail!("Package databases were synchronized separately. Run a full system upgrade (F6) before installing or reinstalling packages.");
        }
        check_database_lock(&self.alpm)?;

        let flags = if reinstall { TransFlag::empty() } else { TransFlag::NEEDED };
        self.alpm.trans_init(flags)?;

        clear_question_refusals();
        let prepare_result = (|| -> Result<i64> {
            for name in targets {
                let pkg = self.alpm.syncdbs().iter()
                    .find_map(|db| db.pkg(name.as_str()).ok())
                    .ok_or_else(|| anyhow::anyhow!("Package not found in syncdbs: {name}"))?;
                self.alpm.trans_add_pkg(pkg)?;
            }

            self.alpm.trans_prepare()
                .map_err(|err| anyhow::anyhow!(with_question_refusals(format!("Transaction prepare failed: {err:?}"))))?;
            if let Some(details) = take_question_refusal_details() {
                anyhow::bail!("Install preview stopped: {details}");
            }

            Ok(self.alpm.trans_add().iter().map(|pkg| pkg.download_size()).sum())
        })();

        let release_result = self.alpm.trans_release();
        match (prepare_result, release_result) {
            (Ok(size), Ok(())) => Ok(size),
            (Err(err), _) => Err(err),
            (Ok(_), Err(err)) => Err(err.into()),
        }
    }

    fn open_install_confirmation(&mut self, targets: Vec<String>, reinstall: bool) {
        if targets.is_empty() {
            return;
        }

        match self.total_download_size_for_packages(&targets, reinstall) {
            Ok(total_size) => {
                let title = if reinstall { "Reinstall" } else { "Install" };
                let action = if reinstall {
                    DialogAction::Reinstall(targets.clone())
                } else {
                    DialogAction::Install(targets.clone())
                };
                self.modal = Some(Modal::Confirm(ConfirmationDialog {
                    title: title.to_string(),
                    packages: targets,
                    additional_removals: Vec::new(),
                    hold_packages: Vec::new(),
                    total_size,
                    selected_button: 0,
                    button_labels: vec!["Proceed".to_string(), "Cancel".to_string()],
                    action,
                }));
            }
            Err(err) => {
                let operation = if reinstall { "Reinstall" } else { "Install" };
                self.modal = Some(Modal::Message(format!("{operation} preview failed: {err}")));
            }
        }
    }

    fn get_pkg_details(&self, pkg_name: &str, installed: bool) -> Vec<String> {
        let pkg_opt = if installed {
            self.alpm.localdb().pkg(pkg_name).ok()
        } else {
            self.alpm
                .syncdbs()
                .iter()
                .find_map(|db| db.pkg(pkg_name).ok())
        };

        if let Some(pkg) = pkg_opt {
            vec![
                format!("Name          : {}", pkg.name()),
                format!("Version       : {}", pkg.version().as_str()),
                format!("Description   : {}", pkg.desc().unwrap_or("None")),
                format!("Architecture  : {}", pkg.arch().unwrap_or("None")),
                format!("URL           : {}", pkg.url().unwrap_or("None")),
                format!(
                    "Licenses      : {}",
                    pkg.licenses().into_iter().collect::<Vec<_>>().join(", ")
                ),
                format!(
                    "Depends On    : {}",
                    pkg.depends()
                        .into_iter()
                        .map(|d| d.name().to_string())
                        .collect::<Vec<_>>()
                        .join(" ")
                ),
                format!(
                    "Installed Size: {:.2} MiB",
                    pkg.isize() as f64 / (1024.0 * 1024.0)
                ),
                format!("Packager      : {}", pkg.packager().unwrap_or("None")),
                format!("Build Date    : {}", pkg.build_date()),
            ]
        } else {
            vec!["Package information not found.".to_string()]
        }
    }

    fn update_progress(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<std::fs::File>>,
        title: &str,
        target: &str,
        step: &str,
        percent: u16,
    ) -> Result<()> {
        let progress = ProgressState {
            title: title.to_string(),
            target: target.to_string(),
            step: step.to_string(),
            percent,
        };
        self.progress = Some(progress.clone());
        ACTIVE_PROGRESS.with(|active| *active.borrow_mut() = Some(progress));
        if let Err(err) = terminal.draw(|f| {
            draw_ui(f, self);
            BACKGROUND_BUFFER.with(|buffer| *buffer.borrow_mut() = Some(f.buffer_mut().clone()));
        }) {
            self.progress = None;
            ACTIVE_PROGRESS.with(|active| *active.borrow_mut() = None);
            BACKGROUND_BUFFER.with(|buffer| *buffer.borrow_mut() = None);
            return Err(err.into());
        }
        thread::sleep(Duration::from_millis(50));
        Ok(())
    }

    // --- Live In-TUI Commit Engine ---
    fn commit_in_tui<F, R>(
        terminal: &mut Terminal<CrosstermBackend<std::fs::File>>,
        _title: &str,
        action: F,
    ) -> Result<R>
    where
        F: FnOnce() -> Result<R>,
    {
        let saved_stdout = unsafe { libc::dup(1) };
        let saved_stderr = unsafe { libc::dup(2) };
        if saved_stdout < 0 || saved_stderr < 0 {
            if saved_stdout >= 0 { unsafe { libc::close(saved_stdout); } }
            if saved_stderr >= 0 { unsafe { libc::close(saved_stderr); } }
            anyhow::bail!("Unable to preserve stdout/stderr for the package transaction");
        }
        let _guard = CommitGuard { saved_stdout, saved_stderr };
        let null_fd = unsafe { libc::open(b"/dev/null\0".as_ptr() as *const _, libc::O_WRONLY) };
        if null_fd < 0 {
            anyhow::bail!("Unable to open /dev/null for transaction output");
        }
        let stdout_result = unsafe { libc::dup2(null_fd, 1) };
        let stderr_result = unsafe { libc::dup2(null_fd, 2) };
        unsafe { libc::close(null_fd); }
        if stdout_result < 0 || stderr_result < 0 {
            anyhow::bail!("Unable to redirect transaction output");
        }
        ACTIVE_TERM.with(|term| term.set(terminal as *mut _));
        action()
    }

    // --- Finding Cached Versions for Downgrade ---
    fn find_cached_versions_for_pkg(&self, pkg_name: &str) -> Vec<CachedVersionItem> {
        let mut candidates = Vec::new();
        let prefix = format!("{}-", pkg_name);

        for dir_str in &self.cache_dirs {
            let dir = Path::new(dir_str);
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if let Some(filename) = path.file_name().and_then(|n| n.to_str()) {
                        if filename.starts_with(&prefix)
                            && (filename.ends_with(".pkg.tar.zst") || filename.ends_with(".pkg.tar.xz"))
                        {
                            let rest = &filename[prefix.len()..];
                            if let Some(first_char) = rest.chars().next() {
                                if first_char.is_ascii_digit() {
                                    if let Some(path_str) = path.to_str() {
                                        if let Ok(loaded_pkg) = self.alpm.pkg_load(path_str, false, SigLevel::NONE) {
                                            if loaded_pkg.name() == pkg_name {
                                                candidates.push(CachedVersionItem {
                                                    path: path.clone(),
                                                    version: loaded_pkg.version().as_str().to_string(),
                                                    size: fs::metadata(&path).map(|metadata| metadata.len().min(i64::MAX as u64) as i64).unwrap_or(0),
                                                    build_date: loaded_pkg.build_date(),
                                                });
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        candidates.sort_by(|a, b| alpm::vercmp(b.version.as_str(), a.version.as_str()));
        candidates.dedup_by(|a, b| a.version == b.version);
        candidates
    }

    fn open_downgrade_view(&mut self) {
        let cur_pkg = match self.left.current_pkg() {
            Some(p) => p,
            None => return,
        };

        let cached = self.find_cached_versions_for_pkg(&cur_pkg.name);
        if cached.is_empty() {
            self.modal = Some(Modal::Message(format!(
                "No cached versions found for '{}' in package caches.",
                cur_pkg.name
            )));
            return;
        }

        self.downgrade_view = Some(DowngradeView {
            pkg_name: cur_pkg.name.clone(),
            installed_version: cur_pkg.version.clone(),
            versions: cached,
            cursor: 0,
        });

        self.active_pane = ActivePane::Right;
    }

    fn alpm_downgrade(
        &mut self,
        pkg_path: std::path::PathBuf,
        lock_version: bool,
        terminal: &mut Terminal<CrosstermBackend<std::fs::File>>,
    ) -> Result<()> {
        check_database_lock(&self.alpm)?;

        let path_str = pkg_path.to_str().ok_or_else(|| anyhow::anyhow!("Invalid package path"))?;
        let pkg = self
            .alpm
            .pkg_load(path_str, true, self.alpm.local_file_siglevel())
            .map_err(|e| anyhow::anyhow!("Failed to load cached archive: {e:?}"))?;

        let pkg_name = pkg.name().to_string();

        clear_question_refusals();
        self.alpm.trans_init(TransFlag::empty())?;
        let add_error = match self.alpm.trans_add_pkg(pkg) {
            Ok(()) => None,
            Err(err) => Some(format!("{err:?}")),
        };
        if let Some(message) = add_error {
            let _ = self.alpm.trans_release();
            anyhow::bail!("Failed to queue package for downgrade: {message}");
        }

        let prep_res = self.alpm.trans_prepare().map_err(|err| {
            with_question_refusals(format!("Downgrade dependency check failed: {err:?}"))
        });
        if let Err(err_msg) = prep_res {
            let _ = self.alpm.trans_release();
            self.progress = None;
            anyhow::bail!("{err_msg}");
        }
        if let Some(details) = take_question_refusal_details() {
            let _ = self.alpm.trans_release();
            anyhow::bail!("Downgrade stopped: {details}");
        }

        if let Err(err) = self.update_progress(
            terminal,
            "Downgrading Package",
            &pkg_name,
            "Committing the verified cached archive...",
            50,
        ) {
            let _ = self.alpm.trans_release();
            self.progress = None;
            return Err(err);
        }

        let commit_result = Self::commit_in_tui(terminal, "Downgrading Package", || {
            self.alpm
                .trans_commit()
                .map_err(|err| anyhow::anyhow!(with_question_refusals(format!("Downgrade commit failed: {err:?}"))))
        });

        if let Err(e) = commit_result {
            let _ = self.alpm.trans_release();
            self.progress = None;
            return Err(e);
        }

        self.alpm.trans_release()?;
        self.progress = None;

        let lock_error = if lock_version
            && self
                .pacman_conf_ignored_pkgs
                .iter()
                .any(|pattern| wildcard_matches(pattern, &pkg_name))
        {
            // pacman.conf already owns this exclusion; do not duplicate it in APC's file.
            None
        } else if lock_version {
            let was_locked = self.locked_pkgs.contains(&pkg_name);
            match self.alpm.add_ignorepkg(pkg_name.as_str()) {
                Ok(()) => {
                    self.locked_pkgs.insert(pkg_name.clone());
                    match self.save_locked_packages() {
                        Ok(()) => None,
                        Err(err) => {
                            if !was_locked {
                                self.locked_pkgs.remove(&pkg_name);
                                let _ = self.alpm.remove_ignorepkg(pkg_name.as_str());
                            }
                            Some(format!("Downgrade succeeded, but the package lock could not be saved: {err}"))
                        }
                    }
                }
                Err(err) => Some(format!("Downgrade succeeded, but the package could not be locked: {err:?}")),
            }
        } else {
            None
        };

        self.refresh_installed()?;
        self.downgrade_view = None;
        self.active_pane = ActivePane::Left;
        flush_events()?;
        if let Some(message) = lock_error {
            anyhow::bail!("{message}");
        }
        Ok(())
    }

    fn alpm_install(
        &mut self,
        targets: Vec<String>,
        reinstall: bool,
        terminal: &mut Terminal<CrosstermBackend<std::fs::File>>,
    ) -> Result<()> {
        check_database_lock(&self.alpm)?;
        clear_question_refusals();

        if self.syncdbs_synced_without_upgrade {
            anyhow::bail!("Package databases were synchronized separately. Run a full system upgrade (F6) before installing or reinstalling packages.");
        }

        // Never delete package archives from configured caches during installation.

        let op_name = if reinstall { "Reinstalling Packages" } else { "Installing Packages" };
        let target_desc = targets.join(", ");

        self.update_progress(terminal, op_name, &target_desc, "Initializing ALPM transaction...", 15)?;

        let flags = if reinstall { TransFlag::empty() } else { TransFlag::NEEDED };
        if let Err(err) = self.alpm.trans_init(flags) {
            self.progress = None;
            return Err(err.into());
        }

        for name in &targets {
            let mut found = false;
            for db in self.alpm.syncdbs() {
                if let Ok(pkg) = db.pkg(name.as_str()) {
                    if let Err(err) = self.alpm.trans_add_pkg(pkg) {
                        let message = format!("{err:?}");
                        let _ = self.alpm.trans_release();
                        self.progress = None;
                        anyhow::bail!("Failed to add {name} to transaction: {message}");
                    }
                    found = true;
                    break;
                }
            }
            if !found {
                let _ = self.alpm.trans_release();
                self.progress = None;
                anyhow::bail!("Package not found in syncdbs: {name}");
            }
        }

        if let Err(err) = self.update_progress(terminal, op_name, &target_desc, "Resolving dependencies & checking disk space...", 45) {
            let _ = self.alpm.trans_release();
            self.progress = None;
            return Err(err);
        }
        let prep_res = self.alpm.trans_prepare().map_err(|err| {
            with_question_refusals(format!("Transaction prepare failed: {err:?}"))
        });
        if let Err(err_msg) = prep_res {
            let _ = self.alpm.trans_release();
            self.progress = None;
            anyhow::bail!("{err_msg}");
        }
        if let Some(details) = take_question_refusal_details() {
            let _ = self.alpm.trans_release();
            self.progress = None;
            anyhow::bail!("Install stopped: {details}");
        }

        let commit_result = Self::commit_in_tui(terminal, op_name, || {
            self.alpm
                .trans_commit()
                .map_err(|err| anyhow::anyhow!(with_question_refusals(format!("Install commit failed: {err:?}"))))
        });

        if let Err(err_msg) = commit_result {
            let _ = self.alpm.trans_release();
            self.progress = None;
            return Err(err_msg);
        }

        self.alpm.trans_release()?;
        self.refresh_installed()?;
        self.right.tagged.clear();
        self.left.tagged.clear();

        self.progress = None;
        flush_events()?;
        Ok(())
    }

    fn alpm_remove(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<std::fs::File>>,
    ) -> Result<()> {
        if let Err(err) = self.update_progress(
            terminal,
            "Removing Packages",
            "Prepared removal transaction",
            "Committing the reviewed -Rns package set...",
            50,
        ) {
            let _ = self.alpm.trans_release();
            self.progress = None;
            return Err(err);
        }
        let commit_result = Self::commit_in_tui(terminal, "Removing Packages", || {
            self.alpm.trans_commit()
                .map_err(|err| anyhow::anyhow!(with_question_refusals(format!("Removal commit failed: {err:?}"))))
        });
        let release_result = self.alpm.trans_release();
        self.progress = None;
        if let Err(err) = commit_result {
            let _ = release_result;
            return Err(err);
        }
        release_result?;
        self.refresh_installed()?;
        self.left.tagged.clear();
        flush_events()?;
        Ok(())
    }

    fn alpm_syncdb(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<std::fs::File>>,
    ) -> Result<()> {
        check_database_lock(&self.alpm)?;
        self.update_progress(terminal, "Syncing Databases", "Configured Mirrors", "Connecting to repository mirrors...", 20)?;

        self.update_progress(terminal, "Syncing Databases", "Core, Extra, Multilib", "Downloading repository databases...", 50)?;
        self.mark_partial_upgrade_pending()?;
        if let Err(err) = self.alpm.syncdbs_mut().update(false) {
            self.progress = None;
            anyhow::bail!("Repository database synchronization failed: {err:?}");
        }

        self.update_progress(terminal, "Syncing Databases", "Local Indices", "Refreshing package lists...", 85)?;
        if let Err(err) = self.refresh_available().and_then(|_| self.refresh_installed()) {
            self.progress = None;
            return Err(err);
        }

        self.update_progress(terminal, "Syncing Databases", "All Repositories", "Synchronization complete!", 100)?;
        thread::sleep(Duration::from_millis(300));
        self.progress = None;
        flush_events()?;
        Ok(())
    }

    fn alpm_check_system_upgrade(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<std::fs::File>>,
    ) -> Result<()> {
        check_database_lock(&self.alpm)?;

        self.update_progress(terminal, "System Upgrade", "Repositories", "Synchronizing mirror databases...", 20)?;
        self.mark_partial_upgrade_pending()?;
        if let Err(err) = self.alpm.syncdbs_mut().update(false) {
            self.progress = None;
            anyhow::bail!("Repository database synchronization failed: {err:?}");
        }
        if let Err(err) = self.refresh_available().and_then(|_| self.refresh_installed()) {
            self.progress = None;
            return Err(err);
        }

        self.update_progress(terminal, "System Upgrade", "Local Packages", "Calculating package updates and new dependencies...", 50)?;

        clear_question_refusals();
        if let Err(e) = self.alpm.trans_init(TransFlag::empty()) {
            self.progress = None;
            anyhow::bail!("Failed to initialize transaction: {e:?}");
        }

        if let Err(e) = self.alpm.sync_sysupgrade(false) {
            let _ = self.alpm.trans_release();
            self.progress = None;
            anyhow::bail!("Sysupgrade calculation failed: {e:?}");
        }

        let prep_res = self.alpm.trans_prepare().map_err(|err| {
            with_question_refusals(format!("Dependency resolution / prepare failed: {err:?}"))
        });
        if let Err(err_msg) = prep_res {
            let _ = self.alpm.trans_release();
            self.progress = None;
            anyhow::bail!("{err_msg}");
        }
        if let Some(details) = take_question_refusal_details() {
            let _ = self.alpm.trans_release();
            self.progress = None;
            anyhow::bail!("System upgrade stopped: {details}");
        }

        let mut updates = Vec::new();
        let mut new_deps = Vec::new();
        let mut total_download_size = 0i64;

        for pkg in self.alpm.trans_add() {
            let name = pkg.name().to_string();
            if self.is_package_locked(&name) {
                continue;
            }

            let size = pkg.download_size();
            total_download_size += size;
            let is_new = self.alpm.localdb().pkg(name.as_str()).is_err();

            let item = UpdateItem {
                name: name.clone(),
                new_version: pkg.version().as_str().to_string(),
                size,
            };

            if is_new {
                new_deps.push(item);
            } else {
                updates.push(item);
            }
        }

        self.progress = None;

        if updates.is_empty() && new_deps.is_empty() {
            self.alpm.trans_release()?;
            self.clear_partial_upgrade_pending()?;
            self.modal = Some(Modal::Message("Your system is already up to date! No updates available.".to_string()));
            return Ok(());
        }

        self.updates_view = Some(SysUpdateInfo {
            updates,
            new_deps,
            total_download_size,
        });
        self.upgrade_selected_button = 0;

        Ok(())
    }

    fn alpm_commit_sysupgrade(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<std::fs::File>>,
    ) -> Result<()> {

        self.system_upgrade_in_progress = true;
        let progress = ProgressState {
            title: "System Upgrade".to_string(),
            target: "Selected system upgrade".to_string(),
            step: "Committing the prepared package transaction...".to_string(),
            percent: 50,
        };
        self.progress = Some(progress.clone());
        ACTIVE_PROGRESS.with(|active| *active.borrow_mut() = Some(progress));
        if let Err(err) = terminal.draw(|f| {
            draw_ui(f, self);
            BACKGROUND_BUFFER.with(|buffer| {
                *buffer.borrow_mut() = Some(f.buffer_mut().clone());
            });
        }) {
            self.system_upgrade_in_progress = false;
            ACTIVE_PROGRESS.with(|active| *active.borrow_mut() = None);
            let _ = self.alpm.trans_release();
            self.updates_view = None;
            self.progress = None;
            return Err(err.into());
        }

        let commit_result = Self::commit_in_tui(terminal, "System Upgrade", || {
            self.alpm
                .trans_commit()
                .map_err(|err| anyhow::anyhow!(with_question_refusals(format!("System upgrade commit failed: {err:?}"))))
        });
        self.system_upgrade_in_progress = false;

        if let Err(e) = commit_result {
            let _ = self.alpm.trans_release();
            self.updates_view = None;
            self.progress = None;
            return Err(e);
        }

        self.progress = None;
        self.alpm.trans_release()?;
        self.clear_partial_upgrade_pending()?;
        self.refresh_installed()?;
        self.refresh_available()?;
        self.updates_view = None;
        flush_events()?;
        Ok(())
    }

    fn refresh_installed(&mut self) -> Result<()> {
        let mut installed = Vec::new();
        for pkg in self.alpm.localdb().pkgs() {
            installed.push(PkgSummary {
                name: pkg.name().to_string(),
                version: pkg.version().as_str().to_string(),
                desc: pkg.desc().unwrap_or("").to_string(),
                size: pkg.isize(),
            });
        }
        installed.sort_by(|a, b| a.name.cmp(&b.name));
        self.left.all_items = installed;
        Ok(())
    }

    fn refresh_available(&mut self) -> Result<()> {
        let mut available = Vec::new();
        for db in self.alpm.syncdbs() {
            for pkg in db.pkgs() {
                available.push(PkgSummary {
                    name: pkg.name().to_string(),
                    version: pkg.version().as_str().to_string(),
                    desc: pkg.desc().unwrap_or("").to_string(),
                    size: pkg.size(),
                });
            }
        }
        available.sort_by(|a, b| a.name.cmp(&b.name));
        available.dedup_by(|a, b| a.name == b.name);
        self.right.all_items = available;
        Ok(())
    }
}

fn check_database_lock(alpm: &Alpm) -> Result<()> {
    let lockfile = Path::new(alpm.lockfile());
    if lockfile.exists() {
        anyhow::bail!(
            "The package database is locked by another package manager or an unfinished transaction: {}. Do not delete this lock automatically; verify that no package operation is running first.",
            lockfile.display()
        );
    }
    Ok(())
}

fn flush_events() -> Result<()> {
    while event::poll(Duration::from_millis(0))? {
        let _ = event::read()?;
    }
    Ok(())
}

fn format_epoch_date(secs: i64) -> String {
    let days = secs / 86400 + 719468;
    let era = (if days >= 0 { days } else { days - 146096 }) / 146097;
    let doe = days - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = y + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02}")
}

// --- UI Rendering ---

fn draw_pane(
    f: &mut Frame,
    area: Rect,
    title: &str,
    state: &PaneState,
    is_active: bool,
    is_left_pane: bool,
    app: &App,
) {
    let border_style = if is_active {
        Style::default().fg(NC_WHITE)
    } else {
        Style::default().fg(NC_CYAN)
    };

    let title_line = Line::from(vec![
        Span::styled(format!(" [ {} ] ", title), Style::default().fg(NC_YELLOW).add_modifier(Modifier::BOLD)),
        if !state.filter.is_empty() {
            Span::styled(format!("[ Filter: {} (Esc to clear) ] ", state.filter), Style::default().fg(NC_WHITE).add_modifier(Modifier::BOLD))
        } else {
            Span::raw("")
        },
    ]);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(border_style)
        .style(Style::default().bg(NC_BLUE).fg(NC_WHITE))
        .title(title_line);

    if !is_left_pane && app.info_mode && app.active_pane == ActivePane::Left {
        let details = if let Some(pkg) = app.left.current_pkg() {
            app.get_pkg_details(&pkg.name, true)
        } else {
            vec!["No package selected".to_string()]
        };

        let info_block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Double)
            .border_style(Style::default().fg(NC_YELLOW))
            .style(Style::default().bg(NC_BLUE).fg(NC_WHITE))
            .title(Span::styled(" [ Quick View: F3/ESC to Close ] ", Style::default().fg(NC_YELLOW).add_modifier(Modifier::BOLD)));

        let text: Vec<Line> = details.into_iter().map(Line::from).collect();
        let p = Paragraph::new(text).block(info_block).wrap(Wrap { trim: false });
        f.render_widget(p, area);
        return;
    }

    if is_left_pane && app.info_mode && app.active_pane == ActivePane::Right {
        let details = if let Some(pkg) = app.right.current_pkg() {
            app.get_pkg_details(&pkg.name, false)
        } else {
            vec!["No package selected".to_string()]
        };

        let info_block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Double)
            .border_style(Style::default().fg(NC_YELLOW))
            .style(Style::default().bg(NC_BLUE).fg(NC_WHITE))
            .title(Span::styled(" [ Quick View: F3/ESC to Close ] ", Style::default().fg(NC_YELLOW).add_modifier(Modifier::BOLD)));

        let text: Vec<Line> = details.into_iter().map(Line::from).collect();
        let p = Paragraph::new(text).block(info_block).wrap(Wrap { trim: false });
        f.render_widget(p, area);
        return;
    }

    // Special Downgrade Version Picker View in Right Pane
    if !is_left_pane {
        if let Some(ref dg_view) = app.downgrade_view {
            let dg_block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Double)
                .border_style(if is_active { Style::default().fg(NC_CYAN) } else { Style::default().fg(NC_YELLOW) })
                .style(Style::default().bg(NC_BLUE).fg(NC_WHITE))
                .title(Span::styled(
                    format!(" [ Cached Versions: {} ] ", dg_view.pkg_name),
                    Style::default().fg(NC_YELLOW).add_modifier(Modifier::BOLD),
                ));

            let visible_rows = (area.height.saturating_sub(3)) as usize;
            let offset = if dg_view.cursor >= visible_rows {
                dg_view.cursor - visible_rows + 1
            } else {
                0
            };

            let header = format!("   {:<22} {:>10}   {:<10}", "Cached Version", "Size", "Build Date");

            let items: Vec<ListItem> = dg_view
                .versions
                .iter()
                .enumerate()
                .skip(offset)
                .take(visible_rows)
                .map(|(idx, v)| {
                    let size_mib = v.size as f64 / (1024.0 * 1024.0);
                    let date_str = format_epoch_date(v.build_date);
                    let mark = if v.version == dg_view.installed_version { " (installed)" } else { "" };
                    let row = format!("   {:<22} {:>9.2}M   {}{}", v.version, size_mib, date_str, mark);

                    if idx == dg_view.cursor && is_active {
                        ListItem::new(row).style(
                            Style::default()
                                .bg(NC_CYAN)
                                .fg(NC_BLACK)
                                .add_modifier(Modifier::BOLD),
                        )
                    } else if v.version == dg_view.installed_version {
                        ListItem::new(row).style(Style::default().fg(NC_GRAY))
                    } else {
                        ListItem::new(row).style(Style::default().fg(NC_WHITE))
                    }
                })
                .collect();

            f.render_widget(dg_block, area);

            if area.height > 3 && area.width > 20 {
                let header_rect = Rect { x: area.x + 1, y: area.y + 1, width: area.width.saturating_sub(2), height: 1 };
                f.render_widget(Paragraph::new(header).style(Style::default().fg(NC_CYAN).bg(NC_BLUE).add_modifier(Modifier::BOLD)), header_rect);

                let list_rect = Rect { x: area.x + 1, y: area.y + 2, width: area.width.saturating_sub(2), height: area.height.saturating_sub(3) };
                f.render_widget(List::new(items), list_rect);
            }

            let status = format!(" {}/{} cached (Enter: Select, Esc: Close) ", dg_view.cursor + 1, dg_view.versions.len());
            let status_rect = Rect { x: area.x + area.width.saturating_sub(status.len() as u16 + 2), y: area.y + area.height - 1, width: status.len() as u16, height: 1 };
            f.render_widget(Paragraph::new(status).style(Style::default().fg(NC_YELLOW).bg(NC_BLUE)), status_rect);
            return;
        }
    }

    // Special System Updates Info Pane (Replaces Repository Packages)
    if !is_left_pane || app.system_upgrade_in_progress {
        if let Some(ref update_info) = app.updates_view {
            let upd_block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Double)
                .border_style(Style::default().fg(NC_YELLOW))
                .style(Style::default().bg(NC_BLUE).fg(NC_WHITE))
                .title(Span::styled(
                    format!(" [ System Updates: {} pkg(s), {} new dep(s) ] ", update_info.updates.len(), update_info.new_deps.len()),
                    Style::default().fg(NC_YELLOW).add_modifier(Modifier::BOLD),
                ));

            let visible_rows = (area.height.saturating_sub(3)) as usize;
            let max_name_w = (area.width.saturating_sub(28) as usize).max(12);

            let header = format!("   {:<name_w$} {:<14} {:>7}", "Package", "New Version", "Size", name_w = max_name_w);

            let mut all_items = Vec::new();
            for dep in &update_info.new_deps {
                let size_mib = dep.size as f64 / (1024.0 * 1024.0);
                let row = format!("* [NEW] {:<name_w$} {:<14} {:>7.2}M", dep.name, dep.new_version, size_mib, name_w = max_name_w.saturating_sub(6));
                all_items.push(ListItem::new(row).style(Style::default().fg(NC_CYAN).add_modifier(Modifier::BOLD)));
            }
            for u in &update_info.updates {
                let size_mib = u.size as f64 / (1024.0 * 1024.0);
                let row = format!("        {:<name_w$} {:<14} {:>7.2}M", u.name, u.new_version, size_mib, name_w = max_name_w.saturating_sub(6));
                all_items.push(ListItem::new(row).style(Style::default().fg(NC_WHITE)));
            }

            f.render_widget(upd_block, area);

            if area.height > 3 && area.width > 20 {
                let header_rect = Rect { x: area.x + 1, y: area.y + 1, width: area.width.saturating_sub(2), height: 1 };
                f.render_widget(Paragraph::new(header).style(Style::default().fg(NC_CYAN).bg(NC_BLUE).add_modifier(Modifier::BOLD)), header_rect);

                let list_rect = Rect { x: area.x + 1, y: area.y + 2, width: area.width.saturating_sub(2), height: area.height.saturating_sub(3) };
                f.render_widget(List::new(all_items.into_iter().take(visible_rows).collect::<Vec<_>>()), list_rect);
            }

            let status = format!(" Total Download: {:.2} MiB ", update_info.total_download_size as f64 / (1024.0 * 1024.0));
            let status_rect = Rect { x: area.x + area.width.saturating_sub(status.len() as u16 + 2), y: area.y + area.height - 1, width: status.len() as u16, height: 1 };
            f.render_widget(Paragraph::new(status).style(Style::default().fg(NC_YELLOW).bg(NC_BLUE)), status_rect);
            return;
        }
    }

    let filtered = state.filtered();
    let total_count = filtered.len();
    let visible_rows = (area.height.saturating_sub(3)) as usize;

    let offset = if state.cursor >= visible_rows {
        state.cursor - visible_rows + 1
    } else {
        0
    };

    let max_name_w = (area.width.saturating_sub(32) as usize).max(12);

    let header_line = if is_left_pane {
        format!("      {:<name_w$} {:<14} {:>7}", "Name", "Version", "Size", name_w = max_name_w)
    } else {
        format!("    {:<name_w$} {:<14} {:>7}", "Name", "Version", "Size", name_w = max_name_w)
    };

    let items: Vec<ListItem> = filtered
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible_rows)
        .map(|(idx, pkg)| {
            let is_tagged = state.tagged.contains(&pkg.name);
            let tag_mark = if is_tagged { "[✔] " } else { "[ ] " };
            let size_mib = pkg.size as f64 / (1024.0 * 1024.0);

            let row_text = if is_left_pane {
                let lock_mark = if app.is_package_locked(&pkg.name) { "\u{f023} " } else { "  " };
                format!(
                    "{}{}{:<name_w$} {:<14} {:>7.2}M",
                    tag_mark,
                    lock_mark,
                    pkg.name,
                    pkg.version,
                    size_mib,
                    name_w = max_name_w
                )
            } else {
                format!(
                    "{}{:<name_w$} {:<14} {:>7.2}M",
                    tag_mark,
                    pkg.name,
                    pkg.version,
                    size_mib,
                    name_w = max_name_w
                )
            };

            if idx == state.cursor && is_active {
                ListItem::new(row_text).style(
                    Style::default()
                        .bg(NC_CYAN)
                        .fg(NC_BLACK)
                        .add_modifier(Modifier::BOLD),
                )
            } else if is_tagged {
                ListItem::new(row_text).style(
                    Style::default()
                        .fg(NC_YELLOW)
                        .add_modifier(Modifier::BOLD),
                )
            } else if is_left_pane && app.is_package_locked(&pkg.name) {
                ListItem::new(row_text).style(
                    Style::default()
                        .fg(NC_YELLOW),
                )
            } else {
                ListItem::new(row_text).style(Style::default().fg(NC_WHITE))
            }
        })
        .collect();

    f.render_widget(block, area);

    if area.height > 3 && area.width > 20 {
        let header_rect = Rect {
            x: area.x + 1,
            y: area.y + 1,
            width: area.width.saturating_sub(2),
            height: 1,
        };
        f.render_widget(
            Paragraph::new(header_line).style(
                Style::default()
                    .fg(NC_CYAN)
                    .bg(NC_BLUE)
                    .add_modifier(Modifier::BOLD),
            ),
            header_rect,
        );

        let list_rect = Rect {
            x: area.x + 1,
            y: area.y + 2,
            width: area.width.saturating_sub(2),
            height: area.height.saturating_sub(3),
        };
        let list = List::new(items);
        f.render_widget(list, list_rect);
    }

    let status_str = format!(" {}/{} pkgs ({} sel) ", if total_count == 0 { 0 } else { state.cursor + 1 }, total_count, state.tagged.len());
    let status_rect = Rect {
        x: area.x + area.width.saturating_sub(status_str.len() as u16 + 2),
        y: area.y + area.height - 1,
        width: status_str.len() as u16,
        height: 1,
    };
    f.render_widget(
        Paragraph::new(status_str).style(Style::default().fg(NC_CYAN).bg(NC_BLUE)),
        status_rect,
    );
}

fn draw_bottom_bar(f: &mut Frame, area: Rect, active: ActivePane, app: &App) {
    if app.system_upgrade_in_progress {
        f.render_widget(
            Paragraph::new(" Please wait. System upgrade in progress. ")
                .style(Style::default().fg(NC_BLACK).bg(NC_GRAY)),
            area,
        );
        return;
    }

    let is_narrow = area.width < 114;

    if app.updates_view.is_some() {
        let choices = [("Proceed", 0), ("Cancel", 1)];
        let mut spans = Vec::new();
        for (label, index) in choices {
            let style = if app.upgrade_selected_button == index {
                Style::default().fg(NC_BLACK).bg(NC_CYAN).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(NC_BLACK).bg(NC_GRAY)
            };
            spans.push(Span::styled(format!(" {label} "), style));
            spans.push(Span::raw(" "));
        }
        f.render_widget(
            Paragraph::new(Line::from(spans)).style(Style::default().bg(NC_GRAY)),
            area,
        );
        return;
    }

    if app.downgrade_view.is_some() {
        let keys = vec![
            ("F1", "Help"),
            ("Enter", "Downgrade"),
            ("Esc", "Cancel"),
        ];
        let mut spans = Vec::new();
        for (key, label) in keys {
            spans.push(Span::styled(format!(" {} ", key), Style::default().fg(NC_BLACK).bg(NC_CYAN).add_modifier(Modifier::BOLD)));
            spans.push(Span::styled(format!("{} ", label), Style::default().fg(NC_BLACK).bg(NC_GRAY)));
            spans.push(Span::raw(" "));
        }
        let p = Paragraph::new(Line::from(spans)).style(Style::default().bg(NC_GRAY));
        f.render_widget(p, area);
        return;
    }

    let current_pkg = app.left.current_pkg();
    let lock_label = match current_pkg {
        Some(pkg)
            if app
                .pacman_conf_ignored_pkgs
                .iter()
                .any(|pattern| wildcard_matches(pattern, &pkg.name)) =>
        {
            "Config"
        },
        Some(pkg) if app.locked_pkgs.contains(&pkg.name) => "Unlock",
        _ => "Lock",
    };

    let f_keys = match active {
        ActivePane::Left => {
            if is_narrow {
                vec![
                    ("F1", "Help"),
                    ("F2", "Reinst"),
                    ("F3", "Info"),
                    ("F4", lock_label),
                    ("F5", "Down"),
                    ("F6", "Upd"),
                    ("F8", "Del"),
                    ("F9", "Sync"),
                    ("F10", "Quit"),
                ]
            } else {
                vec![
                    ("F1", "Help"),
                    ("F2", "Reinstall"),
                    ("F3", "Info"),
                    ("F4", lock_label),
                    ("F5", "Downgrade"),
                    ("F6", "UpdSys"),
                    ("F8", "Delete"),
                    ("F9", "SyncDB"),
                    ("F10", "Quit"),
                ]
            }
        }
        ActivePane::Right => {
            if is_narrow {
                vec![
                    ("F1", "Help"),
                    ("F3", "Info"),
                    ("F5", "Inst"),
                    ("F6", "UpdSys"),
                    ("F9", "Sync"),
                    ("F10", "Quit"),
                ]
            } else {
                vec![
                    ("F1", "Help"),
                    ("F3", "Info"),
                    ("F5", "Install"),
                    ("F6", "UpdSys"),
                    ("F9", "SyncDB"),
                    ("F10", "Quit"),
                ]
            }
        }
    };

    let mut spans = Vec::new();
    for (key, label) in f_keys {
        spans.push(Span::styled(format!(" {} ", key), Style::default().fg(NC_BLACK).bg(NC_CYAN).add_modifier(Modifier::BOLD)));
        spans.push(Span::styled(format!("{} ", label), Style::default().fg(NC_BLACK).bg(NC_GRAY)));
        if !is_narrow {
            spans.push(Span::raw(" "));
        }
    }

    let p = Paragraph::new(Line::from(spans)).style(Style::default().bg(NC_GRAY));
    f.render_widget(p, area);
}

fn draw_dialog_box(f: &mut Frame, width: u16, height: u16) -> Rect {
    let screen = f.area();
    let w = width.min(screen.width.saturating_sub(4));
    let h = height.min(screen.height.saturating_sub(2));

    let x = (screen.width.saturating_sub(w)) / 2;
    let y = (screen.height.saturating_sub(h)) / 2;

    let shadow_rect = Rect {
        x: (x + 2).min(screen.width.saturating_sub(1)),
        y: (y + 1).min(screen.height.saturating_sub(1)),
        width: w.min(screen.width.saturating_sub(x + 2)),
        height: h.min(screen.height.saturating_sub(y + 1)),
    };
    f.render_widget(Block::default().style(Style::default().bg(NC_BLACK)), shadow_rect);

    let dialog_rect = Rect { x, y, width: w, height: h };
    f.render_widget(Clear, dialog_rect);
    dialog_rect
}

fn draw_confirmation_dialog(f: &mut Frame, diag: &ConfirmationDialog) {
    let dialog_rect = draw_dialog_box(f, 70, 18);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(Style::default().fg(NC_WHITE))
        .style(Style::default().bg(NC_GRAY).fg(NC_BLACK))
        .title(Span::styled(
            format!(" {} ", diag.title),
            Style::default().fg(NC_WHITE).bg(NC_DARK_GRAY).add_modifier(Modifier::BOLD),
        ));

    let targets_preview = if diag.packages.len() > 4 {
        format!("{} (+{} more)", diag.packages[..4].join(", "), diag.packages.len() - 4)
    } else {
        diag.packages.join(", ")
    };

    let size_mib = diag.total_size as f64 / (1024.0 * 1024.0);

    let (prompt_text, action_color, size_label) = match diag.action {
        DialogAction::Delete => (
            "You are about to delete:",
            Color::Red,
            format!("Total space to free: {:.2} MiB", size_mib),
        ),
        DialogAction::Install(_) => (
            "You are about to install:",
            NC_BLUE,
            format!("Total package size: {:.2} MiB", size_mib),
        ),
        DialogAction::Reinstall(_) => (
            "You are about to reinstall:",
            Color::Rgb(0, 100, 0),
            format!("Total package size: {:.2} MiB", size_mib),
        ),
        DialogAction::Downgrade { ref target_version, .. } => (
            "Downgrade package:",
            Color::Rgb(0, 100, 0),
            format!("Installed: {}  ──►  Target: {}", diag.packages.first().cloned().unwrap_or_default(), target_version),
        ),
    };

    let mut lines = vec![
        Line::from(""),
        Line::from(Span::styled(prompt_text, Style::default().fg(NC_BLACK).add_modifier(Modifier::BOLD))),
        Line::from(Span::styled(format!("  • {}", targets_preview), Style::default().fg(action_color).add_modifier(Modifier::BOLD))),
    ];

    if !diag.additional_removals.is_empty() {
        let removal_preview = if diag.additional_removals.len() > 5 {
            format!("{} (+{} more)", diag.additional_removals[..5].join(", "), diag.additional_removals.len() - 5)
        } else {
            diag.additional_removals.join(", ")
        };
        lines.push(Line::from(""));
        let sub_title = "Additional packages removed by the prepared -Rns transaction:";
        lines.push(Line::from(Span::styled(sub_title, Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))));
        lines.push(Line::from(Span::styled(format!("  • {}", removal_preview), Style::default().fg(NC_BLACK))));
    }
    if !diag.hold_packages.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("HoldPkg warning: {}", diag.hold_packages.join(", ")),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        size_label,
        Style::default().fg(NC_DARK_GRAY).add_modifier(Modifier::BOLD),
    )));

    if matches!(diag.action, DialogAction::Downgrade { .. }) {
        lines.push(Line::from(Span::styled(
            "Also lock version (\u{f023}) in /etc/pacman.d/IgnorePkg?",
            Style::default().fg(NC_BLACK).add_modifier(Modifier::BOLD),
        )));
    } else {
        lines.push(Line::from(""));
    }
    lines.push(Line::from(""));

    let mut btn_spans = Vec::new();
    for (idx, label) in diag.button_labels.iter().enumerate() {
        let style = if idx == diag.selected_button {
            Style::default().bg(NC_CYAN).fg(NC_BLACK).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(NC_BLACK)
        };
        btn_spans.push(Span::styled(format!("  {}  ", label), style));
        if idx + 1 < diag.button_labels.len() {
            btn_spans.push(Span::raw("    "));
        }
    }
    lines.push(Line::from(btn_spans));

    let p = Paragraph::new(lines).block(block).alignment(Alignment::Center);
    f.render_widget(p, dialog_rect);
}

fn draw_progress_dialog(f: &mut Frame, prog: &ProgressState, state: &PackageProgressState) {
    let dialog_rect = draw_dialog_box(f, 72, 14);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(Style::default().fg(NC_CYAN))
        .style(Style::default().bg(NC_BLUE).fg(NC_WHITE))
        .title(Span::styled(
            format!(" [ Arch Package Commander: {} ] ", prog.title),
            Style::default().fg(NC_YELLOW).add_modifier(Modifier::BOLD),
        ));

    let gauge_width = 38;
    let filled = ((prog.percent as usize * gauge_width) / 100).min(gauge_width);
    let empty = gauge_width - filled;
    let bar_str = format!("[{}{}] {}%", "█".repeat(filled), "░".repeat(empty), prog.percent);

    let mut text = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("Target : ", Style::default().fg(NC_CYAN)),
            Span::styled(&prog.target, Style::default().fg(NC_WHITE).add_modifier(Modifier::BOLD)),
        ]),
        Line::from(vec![
            Span::styled("Status : ", Style::default().fg(NC_CYAN)),
            Span::styled(&prog.step, Style::default().fg(NC_YELLOW)),
        ]),
        Line::from(""),
        Line::from(Span::styled(bar_str, Style::default().fg(NC_CYAN).add_modifier(Modifier::BOLD))),
    ];

    if let Some(active) = state.active_package.as_deref() {
        let percent = state.packages.iter()
            .find(|item| item.name == active)
            .map(|item| item.percent)
            .unwrap_or(state.overall_percent);
        text.push(Line::from(Span::styled(
            format!("Package: {active} ({percent}%)"),
            Style::default().fg(NC_YELLOW),
        )));
        text.push(Line::from(Span::styled(
            format!("Package operations: {}%", state.overall_percent),
            Style::default().fg(NC_WHITE),
        )));
    }
    if let Some(filename) = state.active_download.as_deref() {
        let file_percent = state.downloads.iter()
            .find(|item| item.name == filename)
            .map(|item| item.percent)
            .unwrap_or(state.download_percent);
        text.push(Line::from(Span::styled(
            format!("Download: {filename} ({file_percent}%)"),
            Style::default().fg(NC_CYAN),
        )));
        if state.total_download_bytes > 0 {
            text.push(Line::from(Span::styled(
                format!("All downloads: {}%", state.download_percent),
                Style::default().fg(NC_WHITE),
            )));
            text.push(Line::from(Span::styled(
                format!(
                    "Downloaded: {:.1} / {:.1} MiB",
                    state.downloaded_bytes.max(0) as f64 / (1024.0 * 1024.0),
                    state.total_download_bytes.max(0) as f64 / (1024.0 * 1024.0),
                ),
                Style::default().fg(NC_WHITE),
            )));
        }
    }

    let p = Paragraph::new(text).block(block).alignment(Alignment::Center);
    f.render_widget(p, dialog_rect);
}

fn draw_help_dialog(f: &mut Frame) {
    let dialog_rect = draw_dialog_box(f, 74, 22);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(Style::default().fg(NC_WHITE))
        .style(Style::default().bg(NC_BLUE).fg(NC_WHITE))
        .title(Span::styled(
            " [ Arch Package Commander: Help ] ",
            Style::default().fg(NC_YELLOW).add_modifier(Modifier::BOLD),
        ));

    let help_entries = [
        ("Tab", "Switch active pane (Installed ⇄ Repos)"),
        ("Typing", "Live search / filter active pane instantly"),
        ("Backspace", "Delete last character in filter"),
        ("Esc", "Clear filter / Close Quick View info / Close Downgrade"),
        ("Space / Ins", "Select / tag package ([✔])"),
        ("Enter", "Default action: Uninstall (left) / Install (right)"),
        ("Up / Down", "Move cursor up / down"),
        ("PgUp / PgDn", "Move cursor page up / down"),
        ("Home / End", "Jump to start / end of list"),
        ("F1", "Show this Help table"),
        ("F2", "Reinstall selected package(s)"),
        ("F3", "Quick View package info in opposite pane"),
        ("F4", "Lock / Unlock APC-managed package versions; pacman.conf exclusions are untouched"),
        ("F5", "Downgrade from cache (Left) / Install (Right)"),
        ("F6", "Update system (ALPM equivalent of pacman -Syu)"),
        ("F8", "Remove packages safely (-Rns)"),
        ("F9", "Synchronize databases (pure libalpm)"),
        ("F10", "Quit Arch Package Commander"),
    ];

    let mut lines = vec![Line::from("")];
    for (key, desc) in help_entries {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                format!("{:<13}", key),
                Style::default().fg(NC_YELLOW).add_modifier(Modifier::BOLD),
            ),
            Span::styled(": ", Style::default().fg(NC_CYAN)),
            Span::styled(desc, Style::default().fg(NC_WHITE)),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::raw("                        "),
        Span::styled(
            "[ Press ESC to close ]",
            Style::default().fg(NC_CYAN).add_modifier(Modifier::BOLD),
        ),
    ]));

    let p = Paragraph::new(lines).block(block);
    f.render_widget(p, dialog_rect);
}

fn draw_message_dialog(f: &mut Frame, msg: &str) {
    let screen = f.area();
    let width = 68.min(screen.width.saturating_sub(4));
    let height = 12.min(screen.height.saturating_sub(2));

    let dialog_rect = draw_dialog_box(f, width, height);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(Style::default().fg(Color::Red))
        .style(Style::default().bg(NC_BLUE).fg(NC_WHITE))
        .title(Span::styled(" [ Notice ] ", Style::default().fg(NC_YELLOW)));

    let text = vec![
        Line::from(""),
        Line::from(Span::styled(msg, Style::default().fg(NC_WHITE))),
        Line::from(""),
        Line::from(Span::styled("[ Press ESC/Enter to close ]", Style::default().bg(NC_CYAN).fg(NC_BLACK))),
    ];

    let p = Paragraph::new(text)
        .block(block)
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: false });

    f.render_widget(p, dialog_rect);
}

fn draw_ui(f: &mut Frame, app: &App) {
    let size = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(size);

    let pane_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(chunks[0]);

    draw_pane(
        f,
        pane_chunks[0],
        "Installed Packages",
        &app.left,
        app.active_pane == ActivePane::Left,
        true,
        app,
    );

    draw_pane(
        f,
        pane_chunks[1],
        "Repository Packages",
        &app.right,
        app.active_pane == ActivePane::Right,
        false,
        app,
    );

    draw_bottom_bar(f, chunks[1], app.active_pane, app);

    if let Some(ref modal) = app.modal {
        match modal {
            Modal::Confirm(diag) => draw_confirmation_dialog(f, diag),
            Modal::Help => draw_help_dialog(f),
            Modal::Message(msg) => draw_message_dialog(f, msg),
        }
    } else if let Some(ref prog) = app.progress {
        let package_progress = PACKAGE_PROGRESS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        draw_progress_dialog(f, prog, &package_progress);
    }
}

// --- Main Event Loop ---

fn main() -> Result<()> {
    if unsafe { libc::geteuid() != 0 || libc::getuid() != 0 } {
        let current_exe = std::env::current_exe()?;
        let args: Vec<String> = std::env::args().skip(1).collect();
        let status = std::process::Command::new("sudo")
            .arg(current_exe)
            .args(&args)
            .status()?;
        std::process::exit(status.code().unwrap_or(1));
    }

    let mut app = App::new()?;

    let tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")?;

    let cleanup_tty = tty.try_clone()?;
    enable_raw_mode()?;
    let mut terminal_guard = TerminalGuard { tty: cleanup_tty };
    execute!(&mut terminal_guard.tty, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(tty);
    let mut terminal = Terminal::new(backend)?;

    while !app.should_quit {
        terminal.draw(|f| draw_ui(f, &app))?;

        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }

            // System upgrade confirmation uses the status bar instead of a modal.
            if app.updates_view.is_some() {
                match key.code {
                    KeyCode::Left => app.upgrade_selected_button = 0,
                    KeyCode::Right => app.upgrade_selected_button = 1,
                    KeyCode::Enter => {
                        if app.upgrade_selected_button == 0 {
                            if let Err(e) = app.alpm_commit_sysupgrade(&mut terminal) {
                                app.modal = Some(Modal::Message(format!("Error: {e}")));
                            }
                        } else {
                            let _ = app.alpm.trans_release();
                            app.updates_view = None;
                        }
                    }
                    KeyCode::Esc => {
                        let _ = app.alpm.trans_release();
                        app.updates_view = None;
                    }
                    _ => {}
                }
                continue;
            }

            // Modal Input Handling
            if let Some(mut modal) = app.modal.take() {
                match modal {
                    Modal::Confirm(ref mut diag) => match key.code {
                        KeyCode::Left | KeyCode::BackTab => {
                            let len = diag.button_labels.len();
                            diag.selected_button = (diag.selected_button + len - 1) % len;
                            app.modal = Some(modal);
                        }
                        KeyCode::Right | KeyCode::Tab => {
                            let len = diag.button_labels.len();
                            diag.selected_button = (diag.selected_button + 1) % len;
                            app.modal = Some(modal);
                        }
                        KeyCode::Enter => {
                            let action = std::mem::replace(&mut diag.action, DialogAction::Delete);
                            match action {
                                DialogAction::Delete => {
                                    if diag.selected_button == 0 {
                                        if let Err(e) = app.alpm_remove(&mut terminal) {
                                            app.modal = Some(Modal::Message(format!("Error: {e}")));
                                        }
                                    } else {
                                        let _ = app.alpm.trans_release();
                                        app.progress = None;
                                    }
                                }
                                DialogAction::Install(targets) => {
                                    if diag.selected_button == 0 {
                                        if let Err(e) = app.alpm_install(targets, false, &mut terminal) {
                                            app.modal = Some(Modal::Message(format!("Error: {e}")));
                                        }
                                    }
                                }
                                DialogAction::Reinstall(targets) => {
                                    if diag.selected_button == 0 {
                                        if let Err(e) = app.alpm_install(targets, true, &mut terminal) {
                                            app.modal = Some(Modal::Message(format!("Error: {e}")));
                                        }
                                    }
                                }
                                DialogAction::Downgrade { path, .. } => {
                                    if diag.selected_button == 0 {
                                        // Downgrade & Lock
                                        if let Err(e) = app.alpm_downgrade(path, true, &mut terminal) {
                                            app.modal = Some(Modal::Message(format!("Downgrade Failed: {e}")));
                                        }
                                    } else if diag.selected_button == 1 {
                                        // Downgrade only
                                        if let Err(e) = app.alpm_downgrade(path, false, &mut terminal) {
                                            app.modal = Some(Modal::Message(format!("Downgrade Failed: {e}")));
                                        }
                                    }
                                }
                            }
                        }
                        KeyCode::Esc => {
                            if matches!(&diag.action, DialogAction::Delete) {
                                let _ = app.alpm.trans_release();
                                app.progress = None;
                            }
                        }
                        _ => {
                            app.modal = Some(modal);
                        }
                    },
                    Modal::Help | Modal::Message(_) => {
                        if key.code != KeyCode::Esc && key.code != KeyCode::Enter {
                            app.modal = Some(modal);
                        }
                    }
                }
                continue;
            }

            // Downgrade Version Picker in Right Pane Navigation
            if app.active_pane == ActivePane::Right && app.downgrade_view.is_some() {
                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        if let Some(ref mut dg) = app.downgrade_view {
                            if dg.cursor > 0 {
                                dg.cursor -= 1;
                            }
                        }
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        if let Some(ref mut dg) = app.downgrade_view {
                            if dg.cursor + 1 < dg.versions.len() {
                                dg.cursor += 1;
                            }
                        }
                    }
                    KeyCode::Esc => {
                        app.downgrade_view = None;
                        app.active_pane = ActivePane::Left;
                    }
                    KeyCode::Enter => {
                        if let Some(ref dg) = app.downgrade_view {
                            if let Some(sel) = dg.versions.get(dg.cursor) {
                                let pkg_name = dg.pkg_name.clone();
                                let target_version = sel.version.clone();
                                let path = sel.path.clone();

                                app.modal = Some(Modal::Confirm(ConfirmationDialog {
                                    title: "Downgrade".to_string(),
                                    packages: vec![dg.installed_version.clone()],
                                    additional_removals: Vec::new(),
                                    hold_packages: Vec::new(),
                                    total_size: sel.size,
                                    selected_button: 1,
                                    button_labels: vec![
                                        "Downgrade & Lock".to_string(),
                                        "Downgrade".to_string(),
                                        "Cancel".to_string(),
                                    ],
                                    action: DialogAction::Downgrade {
                                        pkg_name,
                                        target_version,
                                        path,
                                    },
                                }));
                            }
                        }
                    }
                    _ => {}
                }
                continue;
            }

            // Normal Navigation Mode with Always-On Filter
            match key.code {
                KeyCode::Tab => {
                    app.active_pane = match app.active_pane {
                        ActivePane::Left => ActivePane::Right,
                        ActivePane::Right => ActivePane::Left,
                    };
                    app.info_mode = false;
                }
                KeyCode::Esc => {
                    if app.info_mode {
                        app.info_mode = false;
                    } else if app.updates_view.is_some() {
                        let _ = app.alpm.trans_release();
                        app.updates_view = None;
                    } else if app.downgrade_view.is_some() {
                        app.downgrade_view = None;
                        app.active_pane = ActivePane::Left;
                    } else {
                        let cur = match app.active_pane {
                            ActivePane::Left => &mut app.left,
                            ActivePane::Right => &mut app.right,
                        };
                        cur.filter.clear();
                        cur.cursor = 0;
                    }
                }
                KeyCode::Backspace => {
                    let cur = match app.active_pane {
                        ActivePane::Left => &mut app.left,
                        ActivePane::Right => &mut app.right,
                    };
                    cur.filter.pop();
                    cur.cursor = 0;
                }
                KeyCode::Up => {
                    let cur = match app.active_pane {
                        ActivePane::Left => &mut app.left,
                        ActivePane::Right => &mut app.right,
                    };
                    cur.move_up();
                }
                KeyCode::Down => {
                    let cur = match app.active_pane {
                        ActivePane::Left => &mut app.left,
                        ActivePane::Right => &mut app.right,
                    };
                    cur.move_down();
                }
                KeyCode::PageUp => {
                    let cur = match app.active_pane {
                        ActivePane::Left => &mut app.left,
                        ActivePane::Right => &mut app.right,
                    };
                    cur.page_up(15);
                }
                KeyCode::PageDown => {
                    let cur = match app.active_pane {
                        ActivePane::Left => &mut app.left,
                        ActivePane::Right => &mut app.right,
                    };
                    cur.page_down(15);
                }
                KeyCode::Home => {
                    let cur = match app.active_pane {
                        ActivePane::Left => &mut app.left,
                        ActivePane::Right => &mut app.right,
                    };
                    cur.cursor = 0;
                }
                KeyCode::End => {
                    let cur = match app.active_pane {
                        ActivePane::Left => &mut app.left,
                        ActivePane::Right => &mut app.right,
                    };
                    let count = cur.filtered().len();
                    if count > 0 {
                        cur.cursor = count - 1;
                    }
                }
                KeyCode::Char(' ') | KeyCode::Insert => {
                    let cur = match app.active_pane {
                        ActivePane::Left => &mut app.left,
                        ActivePane::Right => &mut app.right,
                    };
                    cur.toggle_tag();
                }
                KeyCode::Enter => {
                    if app.active_pane == ActivePane::Left {
                        let targets: Vec<String> = if !app.left.tagged.is_empty() {
                            app.left.tagged.iter().cloned().collect()
                        } else if let Some(pkg) = app.left.current_pkg() {
                            vec![pkg.name.clone()]
                        } else {
                            vec![]
                        };

                        if !targets.is_empty() {
                            app.open_removal_confirmation(targets);
                        }
                    } else {
                        let targets: Vec<String> = if !app.right.tagged.is_empty() {
                            app.right.tagged.iter().cloned().collect()
                        } else if let Some(pkg) = app.right.current_pkg() {
                            vec![pkg.name.clone()]
                        } else {
                            vec![]
                        };

                        app.open_install_confirmation(targets, false);
                    }
                }
                KeyCode::F(1) => {
                    app.modal = Some(Modal::Help);
                }
                KeyCode::F(2) => {
                    if app.active_pane == ActivePane::Left {
                        let targets: Vec<String> = if !app.left.tagged.is_empty() {
                            app.left.tagged.iter().cloned().collect()
                        } else if let Some(pkg) = app.left.current_pkg() {
                            vec![pkg.name.clone()]
                        } else {
                            vec![]
                        };

                        app.open_install_confirmation(targets, true);
                    }
                }
                KeyCode::F(3) => {
                    app.info_mode = !app.info_mode;
                }
                KeyCode::F(4) => {
                    if app.active_pane == ActivePane::Left {
                        if let Err(e) = app.toggle_lock_current() {
                            app.modal = Some(Modal::Message(format!("Lock Error: {e}")));
                        }
                    }
                }
                KeyCode::F(5) => {
                    if app.active_pane == ActivePane::Left {
                        // Left Pane: Open Downgrade Browser in Right Pane
                        app.open_downgrade_view();
                    } else if app.active_pane == ActivePane::Right {
                        // Right Pane: Install
                        let targets: Vec<String> = if !app.right.tagged.is_empty() {
                            app.right.tagged.iter().cloned().collect()
                        } else if let Some(pkg) = app.right.current_pkg() {
                            vec![pkg.name.clone()]
                        } else {
                            vec![]
                        };

                        app.open_install_confirmation(targets, false);
                    }
                }
                KeyCode::F(6) => {
                    if let Err(e) = app.alpm_check_system_upgrade(&mut terminal) {
                        app.modal = Some(Modal::Message(format!("Update Check Failed: {e}")));
                    }
                }
                KeyCode::F(8) => {
                    if app.active_pane == ActivePane::Left {
                        let targets: Vec<String> = if !app.left.tagged.is_empty() {
                            app.left.tagged.iter().cloned().collect()
                        } else if let Some(pkg) = app.left.current_pkg() {
                            vec![pkg.name.clone()]
                        } else {
                            vec![]
                        };

                        if !targets.is_empty() {
                            app.open_removal_confirmation(targets);
                        }
                    }
                }
                KeyCode::F(9) => {
                    if let Err(e) = app.alpm_syncdb(&mut terminal) {
                        app.modal = Some(Modal::Message(format!("Sync Error: {e}")));
                    }
                }
                KeyCode::F(10) => {
                    app.should_quit = true;
                }
                KeyCode::Char(c) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) && (c.is_alphanumeric() || matches!(c, '-' | '_' | '+' | '.' | '@' | ':')) => {
                    let cur = match app.active_pane {
                        ActivePane::Left => &mut app.left,
                        ActivePane::Right => &mut app.right,
                    };
                    cur.filter.push(c);
                    cur.cursor = 0;
                }
                _ => {}
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn summary(name: &str) -> PkgSummary {
        PkgSummary { name: name.to_string(), version: "1-1".to_string(), desc: String::new(), size: 1 }
    }
    #[test]
    fn tagged_packages_remain_visible_under_filter() {
        let mut pane = PaneState::new(vec![summary("bash"), summary("linux"), summary("vim")]);
        pane.tagged.insert("linux".to_string());
        pane.filter = "vim".to_string();
        let names: Vec<&str> = pane.filtered().iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["linux", "vim"]);
    }
    #[test]
    fn epoch_date_handles_unix_epoch_and_negative_days() {
        assert_eq!(format_epoch_date(0), "1970-01-01");
        assert_eq!(format_epoch_date(86_400), "1970-01-02");
        assert_eq!(format_epoch_date(-86_400), "1969-12-31");
    }
    #[test]
    fn hold_package_patterns_support_globs() {
        assert!(wildcard_matches("linux*", "linux-lts"));
        assert!(wildcard_matches("lib?lpm", "libalpm"));
        assert!(!wildcard_matches("linux", "linux-lts"));
        assert!(is_hold_package("glibc", &["glib*".to_string()]));
    }
    #[test]
    fn siglevel_parser_preserves_global_defaults_and_applies_repo_overrides() {
        let base = SigLevel::PACKAGE | SigLevel::DATABASE;
        let parsed = parse_siglevel(&["DatabaseOptional".to_string()], base).unwrap();
        assert!(parsed.contains(SigLevel::PACKAGE));
        assert!(parsed.contains(SigLevel::DATABASE));
        assert!(parsed.contains(SigLevel::DATABASE_OPTIONAL));

        let combined = parse_siglevel(
            &["Required DatabaseOptional TrustedOnly".to_string()],
            SigLevel::NONE,
        ).unwrap();
        assert!(combined.contains(SigLevel::PACKAGE));
        assert!(combined.contains(SigLevel::DATABASE));
        assert!(combined.contains(SigLevel::DATABASE_OPTIONAL));

        let package_optional =
            parse_siglevel(&["PackageOptional".to_string()], SigLevel::NONE).unwrap();
        assert!(package_optional.contains(SigLevel::PACKAGE));
        assert!(package_optional.contains(SigLevel::PACKAGE_OPTIONAL));

        let never = parse_siglevel(&["Never".to_string()], SigLevel::PACKAGE | SigLevel::DATABASE).unwrap();
        assert!(!never.contains(SigLevel::PACKAGE));
        assert!(!never.contains(SigLevel::PACKAGE_OPTIONAL));
        assert!(!never.contains(SigLevel::DATABASE));
        assert!(!never.contains(SigLevel::DATABASE_OPTIONAL));
    }
    #[test]
    fn pacman_conf_ignore_packages_are_separated_from_apc_managed_entries() {
        let configured = vec![
            "linux linux-lts".to_string(),
            "mesa systemd".to_string(),
            "linux".to_string(),
        ];
        let managed: HashSet<String> = ["linux", "mesa"]
            .into_iter()
            .map(str::to_string)
            .collect();

        let external = parse_external_ignore_packages(&configured, &managed, true);
        assert_eq!(
            external,
            ["linux", "linux-lts", "systemd"]
                .into_iter()
                .map(str::to_string)
                .collect()
        );
        assert!(external.iter().any(|pattern| wildcard_matches(pattern, "linux")));
    }

    #[test]
    fn external_ignore_packages_remain_external_when_managed_file_is_not_included() {
        let configured = vec!["linux linux-lts".to_string()];
        let managed: HashSet<String> = ["linux"].into_iter().map(str::to_string).collect();

        assert_eq!(
            parse_external_ignore_packages(&configured, &managed, false),
            ["linux", "linux-lts"].into_iter().map(str::to_string).collect()
        );
    }

    #[test]
    fn managed_ignore_file_include_detection_handles_globs_and_nested_includes() {
        let root = std::env::temp_dir().join(format!("apc-include-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("conf.d")).unwrap();
        fs::write(root.join("IgnorePkg"), "IgnorePkg = linux\n").unwrap();
        fs::write(
            root.join("pacman.conf"),
            "[options]\nInclude = conf.d/*.conf\n",
        )
        .unwrap();
        fs::write(
            root.join("conf.d/managed.conf"),
            "Include = ../IgnorePkg\n",
        )
        .unwrap();

        assert!(config_includes_file(
            &root.join("pacman.conf"),
            &fs::canonicalize(root.join("IgnorePkg")).unwrap(),
            &mut HashSet::new(),
        ));

        fs::write(
            root.join("pacman.conf"),
            "[options]\nInclude = conf.d/other.conf\n",
        )
        .unwrap();
        fs::write(root.join("conf.d/other.conf"), "Color\n").unwrap();
        assert!(!config_includes_file(
            &root.join("pacman.conf"),
            &fs::canonicalize(root.join("IgnorePkg")).unwrap(),
            &mut HashSet::new(),
        ));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn managed_ignore_file_parser_reads_only_ignorepkg_entries() {
        let parsed = parse_managed_ignore_pkg(
            "# APC-managed locks\nIgnorePkg = linux mesa\nNoUpgrade = /etc/example\n",
        );
        assert_eq!(
            parsed,
            ["linux", "mesa"].into_iter().map(str::to_string).collect()
        );
    }

    #[test]
    fn managed_wildcard_patterns_lock_all_matching_packages() {
        let managed = parse_managed_ignore_pkg(
            "# APC-managed locks\nIgnorePkg = linux-firmware* bash\n",
        );

        for name in [
            "linux-firmware-amdgpu",
            "linux-firmware-intel",
            "linux-firmware-realtek",
            "linux-firmware-whence",
        ] {
            assert!(package_matches_patterns(name, &managed), "{name} should be locked");
            assert!(is_managed_wildcard_lock(name, &managed), "{name} should be covered by the wildcard");
        }

        assert!(package_matches_patterns("bash", &managed));
        assert!(!is_managed_wildcard_lock("bash", &managed));
        assert!(!package_matches_patterns("linux", &managed));
    }

    #[test]
    fn wildcard_unlock_requires_every_matching_installed_package() {
        let installed = vec![
            summary("linux-firmware-amdgpu"),
            summary("linux-firmware-intel"),
            summary("linux-firmware-realtek"),
            summary("linux-firmware-whence"),
            summary("bash"),
        ];
        let patterns: HashSet<String> = ["linux-firmware*".to_string()].into_iter().collect();
        let all_targets: Vec<String> = installed[..4].iter().map(|pkg| pkg.name.clone()).collect();

        assert_eq!(
            managed_wildcard_patterns_to_remove(&all_targets, &installed, &patterns).unwrap(),
            patterns
        );

        for selected_count in [1, 3] {
            let partial: Vec<String> = installed[..selected_count]
                .iter()
                .map(|pkg| pkg.name.clone())
                .collect();
            assert!(
                managed_wildcard_patterns_to_remove(&partial, &installed, &patterns).is_err(),
                "selecting {selected_count} of four packages must be rejected"
            );
        }
    }

    #[test]
    fn wildcard_unlock_rejects_mixed_selection() {
        let installed = vec![
            summary("linux-firmware-amdgpu"),
            summary("linux-firmware-intel"),
            summary("bash"),
        ];
        let patterns: HashSet<String> = ["linux-firmware*".to_string()].into_iter().collect();
        let mixed = vec![
            "linux-firmware-amdgpu".to_string(),
            "linux-firmware-intel".to_string(),
            "bash".to_string(),
        ];

        assert!(
            managed_wildcard_patterns_to_remove(&mixed, &installed, &patterns).is_err()
        );
    }

    #[test]
    fn repository_usage_parser_defaults_to_all_and_accepts_restrictions() {
        assert_eq!(parse_repo_usage(&[]).unwrap(), Usage::ALL);
        assert_eq!(
            parse_repo_usage(&["Sync Install".to_string()]).unwrap(),
            Usage::SYNC | Usage::INSTALL
        );
    }
}
