use alpm::{Alpm, SigLevel, TransFlag};
use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
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
                .filter(|p| p.name.to_lowercase().contains(&f))
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
    Delete(Vec<String>),
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
    cascade: Vec<String>,
    total_size: i64,
    selected_button: usize,
    button_labels: Vec<String>,
    action: DialogAction,
}

struct ProgressState {
    title: String,
    target: String,
    step: String,
    percent: u16,
}

struct PackageProgressItem {
    name: String,
    percent: u16,
}

#[derive(Default)]
struct PackageProgressState {
    packages: Vec<PackageProgressItem>,
    active_package: Option<String>,
    overall_percent: u16,
}

static PACKAGE_PROGRESS: std::sync::Mutex<PackageProgressState> =
    std::sync::Mutex::new(PackageProgressState {
        packages: Vec::new(),
        active_package: None,
        overall_percent: 0,
    });

enum Modal {
    Confirm(ConfirmationDialog),
    Help,
    Message(String),
}

struct RootGuard;

impl RootGuard {
    pub fn acquire() -> Result<Self> {
        Ok(Self)
    }
}

struct App {
    alpm: Alpm,
    active_pane: ActivePane,
    left: PaneState,
    right: PaneState,
    info_mode: bool,
    locked_pkgs: HashSet<String>,
    updates_view: Option<SysUpdateInfo>,
    upgrade_selected_button: usize,
    downgrade_view: Option<DowngradeView>,
    cache_dirs: Vec<String>,
    modal: Option<Modal>,
    progress: Option<ProgressState>,
    system_upgrade_in_progress: bool,
    should_quit: bool,
}

impl App {
    fn new() -> Result<Self> {
        let pacconf = pacmanconf::Config::new().unwrap_or_default();
        let mut alpm = Alpm::new(
            pacconf.root_dir.as_str(),
            pacconf.db_path.as_str(),
        )?;

        // Connect pacman.log
        let _ = alpm.set_logfile(pacconf.log_file.as_str());

        // 1. Ensure the official system cache is ALWAYS the primary write directory
        let default_cache = "/var/cache/pacman/pkg/";
        let _ = alpm.add_cachedir(default_cache);

        let mut cache_dirs = vec![default_cache.to_string()];

        // 2. Register any other custom/local cachedirs as secondary caches
        for cachedir in &pacconf.cache_dir {
            let clean_dir = cachedir.trim_end_matches('/');
            if clean_dir != "/var/cache/pacman/pkg" {
                let _ = alpm.add_cachedir(cachedir.as_str());
            }
            if !cache_dirs.contains(cachedir) {
                cache_dirs.push(cachedir.clone());
            }
        }

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

        let arch = pacconf
            .architecture
            .first()
            .cloned()
            .unwrap_or_else(|| "x86_64".to_string());

        // 1. Auto-register local file:// repositories as secondary caches first
        // (prevents conflicting mutable borrows of alpm)
        for repo in &pacconf.repos {
            for server in &repo.servers {
                let expanded = server
                    .replace("$repo", &repo.name)
                    .replace("$arch", &arch);
                if let Some(local_path) = expanded.strip_prefix("file://") {
                    let clean_path = local_path.trim_end_matches('/');
                    if clean_path != "/var/cache/pacman/pkg" && Path::new(clean_path).is_dir() {
                        let _ = alpm.add_cachedir(clean_path);
                        if !cache_dirs.contains(&clean_path.to_string()) {
                            cache_dirs.push(clean_path.to_string());
                        }
                    }
                }
            }
        }

        // 2. Register syncdb databases
        for repo in &pacconf.repos {
            let sig_level = SigLevel::PACKAGE_OPTIONAL | SigLevel::DATABASE_OPTIONAL;
            if let Ok(db) = alpm.register_syncdb_mut(repo.name.as_str(), sig_level) {
                for server in &repo.servers {
                    let expanded = server
                        .replace("$repo", &repo.name)
                        .replace("$arch", &arch);
                    let _ = db.add_server(expanded.as_str());
                }
            }
        }

        let locked_pkgs = Self::load_locked_packages();
        for pkg in &locked_pkgs {
            let _ = alpm.add_ignorepkg(pkg.as_str());
        }

        let mut app = Self {
            alpm,
            active_pane: ActivePane::Left,
            left: PaneState::new(Vec::new()),
            right: PaneState::new(Vec::new()),
            info_mode: false,
            locked_pkgs,
            updates_view: None,
            upgrade_selected_button: 0,
            downgrade_view: None,
            cache_dirs,
            modal: None,
            progress: None,
            system_upgrade_in_progress: false,
            should_quit: false,
        };

        app.refresh_installed()?;
        app.refresh_available()?;
        Ok(app)
    }

    fn load_locked_packages() -> HashSet<String> {
        let mut locked = HashSet::new();
        if let Ok(content) = fs::read_to_string(IGNORE_PKG_FILE) {
            for line in content.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with('#') || trimmed.is_empty() {
                    continue;
                }
                if let Some((key, val)) = trimmed.split_once('=') {
                    if key.trim().eq_ignore_ascii_case("IgnorePkg") {
                        for pkg in val.split_whitespace() {
                            locked.insert(pkg.trim().to_string());
                        }
                    }
                }
            }
        }
        locked
    }

    fn save_locked_packages(&self) -> Result<()> {
        let dir = Path::new("/etc/pacman.d");
        if !dir.exists() {
            let _ = fs::create_dir_all(dir);
        }
        let mut list: Vec<&str> = self.locked_pkgs.iter().map(|s| s.as_str()).collect();
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
                if let Some(mut stdin) = child.stdin.take() {
                    stdin.write_all(content.as_bytes())?;
                }
                child.wait()?;
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

        let any_unlocked = targets.iter().any(|t| !self.locked_pkgs.contains(t));

        for t in &targets {
            if any_unlocked {
                self.locked_pkgs.insert(t.clone());
                let _ = self.alpm.add_ignorepkg(t.as_str());
            } else {
                self.locked_pkgs.remove(t);
                let _ = self.alpm.remove_ignorepkg(t.as_str());
            }
        }

        self.save_locked_packages()?;
        self.left.tagged.clear();
        Ok(())
    }

    fn get_removal_cascade(&self, targets: &[String]) -> (Vec<String>, i64) {
        let mut to_remove = HashSet::new();
        let mut queue = Vec::new();

        for t in targets {
            to_remove.insert(t.clone());
            queue.push(t.clone());
        }

        while let Some(current) = queue.pop() {
            for pkg in self.alpm.localdb().pkgs() {
                let name = pkg.name().to_string();
                if to_remove.contains(&name) {
                    continue;
                }
                let depends_on_current = pkg.depends().into_iter().any(|dep| dep.name() == current);
                if depends_on_current {
                    to_remove.insert(name.clone());
                    queue.push(name);
                }
            }
        }

        let mut total_size = 0i64;
        let mut cascade_list = Vec::new();
        for name in &to_remove {
            if let Ok(pkg) = self.alpm.localdb().pkg(name.as_str()) {
                total_size += pkg.isize();
            }
            if !targets.contains(name) {
                cascade_list.push(name.clone());
            }
        }
        cascade_list.sort();
        (cascade_list, total_size)
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
        self.progress = Some(ProgressState {
            title: title.to_string(),
            target: target.to_string(),
            step: step.to_string(),
            percent,
        });
        terminal.draw(|f| draw_ui(f, self))?;
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
        let null_fd = unsafe { libc::open(b"/dev/null\0".as_ptr() as *const _, libc::O_WRONLY) };
        let saved_stdout = unsafe { libc::dup(1) };
        let saved_stderr = unsafe { libc::dup(2) };
        if null_fd >= 0 {
            unsafe {
                libc::dup2(null_fd, 1);
                libc::dup2(null_fd, 2);
                libc::close(null_fd);
            }
        }

        ACTIVE_TERM.with(|t| t.set(terminal as *mut _));


        let result = action();

        ACTIVE_TERM.with(|t| t.set(std::ptr::null_mut()));

        if saved_stdout >= 0 {
            unsafe {
                libc::dup2(saved_stdout, 1);
                libc::dup2(saved_stderr, 2);
                libc::close(saved_stdout);
                libc::close(saved_stderr);
            }
        }


        result
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
                                                    size: loaded_pkg.isize(),
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
        let _guard = RootGuard::acquire()?;
        check_and_clean_stale_lock();

        let path_str = pkg_path.to_str().ok_or_else(|| anyhow::anyhow!("Invalid package path"))?;
        let pkg = self
            .alpm
            .pkg_load(path_str, true, SigLevel::NONE)
            .map_err(|e| anyhow::anyhow!("Failed to load cached archive: {e:?}"))?;

        let pkg_name = pkg.name().to_string();

        self.alpm.trans_init(TransFlag::empty())?;
        self.alpm
            .trans_add_pkg(pkg)
            .map_err(|e| anyhow::anyhow!("Failed to queue package for downgrade: {e:?}"))?;

        let prep_res = self.alpm.trans_prepare().map_err(|e| format!("{e:?}"));
        if let Err(err_msg) = prep_res {
            let _ = self.alpm.trans_release();
            self.progress = None;
            anyhow::bail!("Downgrade dependency check failed: {err_msg}");
        }

        let commit_result = Self::commit_in_tui(terminal, "Downgrading Package", || {
            self.alpm
                .trans_commit()
                .map_err(|e| anyhow::anyhow!("Downgrade commit failed: {e:?}"))
        });

        if let Err(e) = commit_result {
            let _ = self.alpm.trans_release();
            self.progress = None;
            return Err(e);
        }

        self.alpm.trans_release()?;

        if lock_version {
            self.locked_pkgs.insert(pkg_name.clone());
            let _ = self.alpm.add_ignorepkg(pkg_name.as_str());
            let _ = self.save_locked_packages();
        }

        self.refresh_installed()?;
        self.downgrade_view = None;
        self.active_pane = ActivePane::Left;
        flush_events()?;
        Ok(())
    }

    fn alpm_install(
        &mut self,
        targets: Vec<String>,
        reinstall: bool,
        terminal: &mut Terminal<CrosstermBackend<std::fs::File>>,
    ) -> Result<()> {
        let _guard = RootGuard::acquire()?;
        check_and_clean_stale_lock();

        // Purge stale archives from global cache to avoid checksum collisions
        let pkg_cache = Path::new("/var/cache/pacman/pkg");
        if let Ok(entries) = fs::read_dir(pkg_cache) {
            let prefixes: Vec<String> = targets.iter().map(|t| format!("{t}-")).collect();
            for entry in entries.flatten() {
                let path = entry.path();
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if (name.ends_with(".pkg.tar.zst") || name.ends_with(".pkg.tar.xz"))
                        && prefixes.iter().any(|p| name.starts_with(p))
                    {
                        let _ = fs::remove_file(&path);
                    }
                }
            }
        }

        let op_name = if reinstall { "Reinstalling Packages" } else { "Installing Packages" };
        let target_desc = targets.join(", ");

        self.update_progress(terminal, op_name, &target_desc, "Initializing ALPM transaction...", 15)?;

        let flags = if reinstall { TransFlag::empty() } else { TransFlag::NEEDED };
        self.alpm.trans_init(flags)?;

        for name in &targets {
            let mut found = false;
            for db in self.alpm.syncdbs() {
                if let Ok(pkg) = db.pkg(name.as_str()) {
                    self.alpm
                        .trans_add_pkg(pkg)
                        .map_err(|e| anyhow::anyhow!("Failed to add {name} to transaction: {e:?}"))?;
                    found = true;
                    break;
                }
            }
            if !found {
                let _ = self.alpm.trans_release();
                anyhow::bail!("Package not found in syncdbs: {name}");
            }
        }

        self.update_progress(terminal, op_name, &target_desc, "Resolving dependencies & checking disk space...", 45)?;
        let prep_res = self.alpm.trans_prepare().map_err(|e| format!("{e:?}"));
        if let Err(err_msg) = prep_res {
            let _ = self.alpm.trans_release();
            self.progress = None;
            anyhow::bail!("Transaction prepare failed: {err_msg}");
        }

        let commit_result = Self::commit_in_tui(terminal, op_name, || {
            self.alpm
                .trans_commit()
                .map_err(|e| anyhow::anyhow!("Install commit failed: {e:?}"))
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
        targets: Vec<String>,
        terminal: &mut Terminal<CrosstermBackend<std::fs::File>>,
    ) -> Result<()> {
        let _guard = RootGuard::acquire()?;
        check_and_clean_stale_lock();

        let target_desc = targets.join(", ");

        self.update_progress(terminal, "Deleting Packages", &target_desc, "Initializing removal transaction...", 15)?;
        self.alpm.trans_init(TransFlag::RECURSE | TransFlag::UNNEEDED)?;

        for name in &targets {
            if let Ok(pkg) = self.alpm.localdb().pkg(name.as_str()) {
                self.alpm
                    .trans_remove_pkg(pkg)
                    .map_err(|e| anyhow::anyhow!("Failed to mark {name} for removal: {e:?}"))?;
            }
        }

        self.update_progress(terminal, "Deleting Packages", &target_desc, "Resolving dependency cascades (-Rns)...", 50)?;
        let prep_res = self.alpm.trans_prepare().map_err(|e| format!("{e:?}"));
        if let Err(err_msg) = prep_res {
            let _ = self.alpm.trans_release();
            self.progress = None;
            anyhow::bail!("Transaction prepare failed: {err_msg}");
        }

        let commit_result = Self::commit_in_tui(terminal, "Deleting Packages", || {
            self.alpm
                .trans_commit()
                .map_err(|e| anyhow::anyhow!("Removal commit failed: {e:?}"))
        });

        if let Err(err_msg) = commit_result {
            let _ = self.alpm.trans_release();
            self.progress = None;
            return Err(err_msg);
        }

        self.alpm.trans_release()?;
        self.refresh_installed()?;
        self.left.tagged.clear();

        self.progress = None;
        flush_events()?;
        Ok(())
    }

    fn alpm_syncdb(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<std::fs::File>>,
    ) -> Result<()> {
        self.update_progress(terminal, "Syncing Databases", "Configured Mirrors", "Connecting to repository mirrors...", 20)?;

        self.update_progress(terminal, "Syncing Databases", "Core, Extra, Multilib", "Downloading repository databases...", 50)?;
        let _ = self.alpm.syncdbs_mut().update(false);

        self.update_progress(terminal, "Syncing Databases", "Local Indices", "Refreshing package lists...", 85)?;
        self.refresh_available()?;
        self.refresh_installed()?;

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
        let _guard = RootGuard::acquire()?;
        check_and_clean_stale_lock();

        self.update_progress(terminal, "System Upgrade", "Repositories", "Synchronizing mirror databases...", 20)?;
        let _ = self.alpm.syncdbs_mut().update(false);
        self.refresh_available()?;
        self.refresh_installed()?;

        self.update_progress(terminal, "System Upgrade", "Local Packages", "Calculating package updates and new dependencies...", 50)?;

        if let Err(e) = self.alpm.trans_init(TransFlag::empty()) {
            self.progress = None;
            anyhow::bail!("Failed to initialize transaction: {e:?}");
        }

        if let Err(e) = self.alpm.sync_sysupgrade(false) {
            let _ = self.alpm.trans_release();
            self.progress = None;
            anyhow::bail!("Sysupgrade calculation failed: {e:?}");
        }

        let prep_res = self.alpm.trans_prepare().map_err(|e| format!("{e:?}"));
        if let Err(err_msg) = prep_res {
            let _ = self.alpm.trans_release();
            self.progress = None;
            anyhow::bail!("Dependency resolution / prepare failed: {err_msg}");
        }

        let mut updates = Vec::new();
        let mut new_deps = Vec::new();
        let mut total_download_size = 0i64;

        for pkg in self.alpm.trans_add() {
            let name = pkg.name().to_string();
            if self.locked_pkgs.contains(&name) {
                continue;
            }

            let size = pkg.isize();
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
            let _ = self.alpm.trans_release();
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
        let _guard = RootGuard::acquire()?;

        let commit_result = Self::commit_in_tui(terminal, "System Upgrade", || {
            self.alpm
                .trans_commit()
                .map_err(|e| anyhow::anyhow!("System upgrade commit failed: {e:?}"))
        });

        if let Err(e) = commit_result {
            let _ = self.alpm.trans_release();
            self.updates_view = None;
            self.progress = None;
            return Err(e);
        }

        self.alpm.trans_release()?;
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
                    size: pkg.isize(),
                });
            }
        }
        available.sort_by(|a, b| a.name.cmp(&b.name));
        available.dedup_by(|a, b| a.name == b.name);
        self.right.all_items = available;
        Ok(())
    }
}

fn check_and_clean_stale_lock() {
    let lck_path = Path::new("/var/lib/pacman/db.lck");
    if lck_path.exists() {
        let is_running = std::process::Command::new("pgrep")
            .arg("-x")
            .arg("pacman")
            .output()
            .map(|o| !o.stdout.is_empty())
            .unwrap_or(false);

        if !is_running {
            let _ = fs::remove_file(lck_path);
        }
    }
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
    if !is_left_pane {
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
                let lock_mark = if app.locked_pkgs.contains(&pkg.name) { "\u{f023} " } else { "  " };
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
            } else if is_left_pane && app.locked_pkgs.contains(&pkg.name) {
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

    let is_cur_locked = app.left.current_pkg().map(|p| app.locked_pkgs.contains(&p.name)).unwrap_or(false);
    let lock_label = if is_cur_locked { "Unlock" } else { "Lock" };

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
        x: (x + 2).min(screen.width - 1),
        y: (y + 1).min(screen.height - 1),
        width: w.min(screen.width.saturating_sub(x + 2)),
        height: h.min(screen.height.saturating_sub(y + 1)),
    };
    f.render_widget(Block::default().style(Style::default().bg(NC_BLACK)), shadow_rect);

    let dialog_rect = Rect { x, y, width: w, height: h };
    f.render_widget(Clear, dialog_rect);
    dialog_rect
}

fn draw_confirmation_dialog(f: &mut Frame, diag: &ConfirmationDialog) {
    let dialog_rect = draw_dialog_box(f, 70, 16);

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
        DialogAction::Delete(_) => (
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

    if !diag.cascade.is_empty() {
        let cascade_preview = if diag.cascade.len() > 5 {
            format!("{} (+{} more)", diag.cascade[..5].join(", "), diag.cascade.len() - 5)
        } else {
            diag.cascade.join(", ")
        };
        lines.push(Line::from(""));
        let sub_title = "Dependent packages that will also be removed:";
        lines.push(Line::from(Span::styled(sub_title, Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))));
        lines.push(Line::from(Span::styled(format!("  • {}", cascade_preview), Style::default().fg(NC_BLACK))));
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

fn draw_progress_dialog(f: &mut Frame, prog: &ProgressState) {
    let dialog_rect = draw_dialog_box(f, 62, 11);

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

    let text = vec![
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
        ("F4", "Lock / Unlock package version (\u{f023} via /etc/pacman.d/IgnorePkg)"),
        ("F5", "Downgrade from cache (Left) / Install (Right)"),
        ("F6", "Update system (ALPM equivalent of pacman -Syu)"),
        ("F8", "Delete selected package(s) (-Rns cascade)"),
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
        draw_progress_dialog(f, prog);
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
        std::process::exit(status.code().unwrap_or(0));
    }

    check_and_clean_stale_lock();

    let mut app = match App::new() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("Failed to initialize ALPM: {e}");
            return Ok(());
        }
    };

    let tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")?;

    enable_raw_mode()?;
    let mut tty_clone = tty.try_clone()?;
    execute!(tty_clone, EnterAlternateScreen)?;
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
                            let action = std::mem::replace(&mut diag.action, DialogAction::Delete(Vec::new()));
                            match action {
                                DialogAction::Delete(targets) => {
                                    if diag.selected_button == 0 {
                                        if let Err(e) = app.alpm_remove(targets, &mut terminal) {
                                            app.modal = Some(Modal::Message(format!("Error: {e}")));
                                        }
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
                        KeyCode::Esc => {}
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
                                    cascade: Vec::new(),
                                    total_size: sel.size,
                                    selected_button: 0,
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
                            let (cascade, total_size) = app.get_removal_cascade(&targets);
                            app.modal = Some(Modal::Confirm(ConfirmationDialog {
                                title: "Delete".to_string(),
                                packages: targets.clone(),
                                cascade,
                                total_size,
                                selected_button: 0,
                                button_labels: vec!["Proceed".to_string(), "Cancel".to_string()],
                                action: DialogAction::Delete(targets),
                            }));
                        }
                    } else {
                        let targets: Vec<String> = if !app.right.tagged.is_empty() {
                            app.right.tagged.iter().cloned().collect()
                        } else if let Some(pkg) = app.right.current_pkg() {
                            vec![pkg.name.clone()]
                        } else {
                            vec![]
                        };

                        if !targets.is_empty() {
                            let total_size: i64 = targets.iter().filter_map(|t| {
                                app.right.all_items.iter().find(|p| &p.name == t).map(|p| p.size)
                            }).sum();

                            app.modal = Some(Modal::Confirm(ConfirmationDialog {
                                title: "Install".to_string(),
                                packages: targets.clone(),
                                cascade: Vec::new(),
                                total_size,
                                selected_button: 0,
                                button_labels: vec!["Proceed".to_string(), "Cancel".to_string()],
                                action: DialogAction::Install(targets),
                            }));
                        }
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

                        if !targets.is_empty() {
                            let total_size: i64 = targets.iter().filter_map(|t| {
                                app.left.all_items.iter().find(|p| &p.name == t).map(|p| p.size)
                            }).sum();

                            app.modal = Some(Modal::Confirm(ConfirmationDialog {
                                title: "Reinstall".to_string(),
                                packages: targets.clone(),
                                cascade: Vec::new(),
                                total_size,
                                selected_button: 0,
                                button_labels: vec!["Proceed".to_string(), "Cancel".to_string()],
                                action: DialogAction::Reinstall(targets),
                            }));
                        }
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

                        if !targets.is_empty() {
                            let total_size: i64 = targets.iter().filter_map(|t| {
                                app.right.all_items.iter().find(|p| &p.name == t).map(|p| p.size)
                            }).sum();

                            app.modal = Some(Modal::Confirm(ConfirmationDialog {
                                title: "Install".to_string(),
                                packages: targets.clone(),
                                cascade: Vec::new(),
                                total_size,
                                selected_button: 0,
                                button_labels: vec!["Proceed".to_string(), "Cancel".to_string()],
                                action: DialogAction::Install(targets),
                            }));
                        }
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
                            let (cascade, total_size) = app.get_removal_cascade(&targets);
                            app.modal = Some(Modal::Confirm(ConfirmationDialog {
                                title: "Delete".to_string(),
                                packages: targets.clone(),
                                cascade,
                                total_size,
                                selected_button: 0,
                                button_labels: vec!["Proceed".to_string(), "Cancel".to_string()],
                                action: DialogAction::Delete(targets),
                            }));
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
                KeyCode::Char(c) if c.is_alphanumeric() || matches!(c, '-' | '_' | '+' | '.' | '@' | ':') => {
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

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    Ok(())
}
