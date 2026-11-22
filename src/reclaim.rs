//! Release of allocations whose worktree is gone, for every project in the
//! ledger. One decision ([`judge`]) serves the `sync` slow path, `portool
//! prune`, `portool prune --all` and the automatic sweep.
//!
//! A wrongly released allocation hands the same ports to two live worktrees,
//! so an entry is released only on positive evidence, all of it:
//!
//! 1. it is not pinned;
//! 2. its own directory is *verifiably* gone ([`presence`]): `lstat` says
//!    "not found", and the nearest existing ancestor is an ordinary,
//!    non-empty directory on the same device as its parent that holds no
//!    mount point and is not a conventional mount directory -- anything else
//!    may be a volume that is merely not mounted;
//! 3. git agrees: the repository is itself verifiably gone, or git no
//!    longer lists the worktree, or lists it as `prunable` and not `locked`;
//! 4. its ports are unused right now (checked last, under the ledger lock).
//!
//! Free ports alone never release anything, and neither does a missing
//! repository: each entry needs its own directory gone.
//!
//! The sweep must not be able to stall the ledger. Everything that can block
//! on someone else's files (`lstat`, `readdir`, `git`) therefore runs
//! *before* the ledger lock is taken, in a helper thread with a time budget
//! ([`plan`]); under the lock, [`Plan::apply`] only compares in-memory
//! entries and probes ports, and rides on the save the command makes anyway.

use crate::gitctx::{self, Listed};
use crate::ports;
use crate::registry::{Registry, WorktreeEntry};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How long `git worktree list` for *another* project may take in the
/// automatic sweep. A healthy call takes tens of milliseconds.
pub const GIT_TIMEOUT: Duration = Duration::from_secs(2);

/// Upper bound on the time the automatic sweep adds to one command.
pub const BUDGET: Duration = Duration::from_secs(3);

/// The same bounds for `portool prune --all`, where the user is waiting for
/// exactly this answer.
pub const MANUAL_GIT_TIMEOUT: Duration = Duration::from_secs(10);

/// A project whose check failed or timed out is not checked again by the
/// automatic sweep for this long.
pub const RETRY_AFTER: Duration = Duration::from_secs(3600);

/// Remembers failed checks, next to the ledger. Removed when its last entry
/// expires, or by the next ledger save once it is [`RETRY_AFTER`] old.
pub const MEMORY_FILE: &str = "stale-check-failed.json";

/// Helper threads that may be stuck in a blocking call at once (they cannot
/// be cancelled). Beyond this the sweep is skipped.
const MAX_HELPERS: usize = 2;
static HELPERS: AtomicUsize = AtomicUsize::new(0);

/// What an `lstat` of a path said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stat {
    /// A directory, with the device it is on.
    Dir(u64),
    /// Something that is not a directory (file, symlink, ...).
    NotDir,
    NotFound,
    /// Permission denied, I/O error ...: not proof of absence.
    Unknown,
}

/// What a directory holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Children {
    pub count: usize,
    /// The device of every child that is a directory.
    pub dir_devices: Vec<u64>,
}

/// The filesystem questions the "gone" decision asks.
pub trait Fs {
    fn stat(&self, path: &Path) -> Stat;
    /// `None` when the directory cannot be read completely.
    fn children(&self, dir: &Path) -> Option<Children>;
}

pub struct RealFs;

impl Fs for RealFs {
    fn stat(&self, path: &Path) -> Stat {
        use std::os::unix::fs::MetadataExt;
        match std::fs::symlink_metadata(path) {
            Ok(md) if md.is_dir() => Stat::Dir(md.dev()),
            Ok(_) => Stat::NotDir,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Stat::NotFound,
            Err(_) => Stat::Unknown,
        }
    }

    fn children(&self, dir: &Path) -> Option<Children> {
        use std::os::unix::fs::MetadataExt;
        let mut out = Children {
            count: 0,
            dir_devices: Vec::new(),
        };
        for entry in std::fs::read_dir(dir).ok()? {
            let entry = entry.ok()?;
            out.count += 1;
            // Only a directory can be a mount point; `file_type` comes with
            // the directory entry, so files cost no `lstat`.
            if entry.file_type().ok()?.is_dir() {
                match std::fs::symlink_metadata(entry.path()) {
                    Ok(md) => out.dir_devices.push(md.dev()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(_) => return None,
                }
            }
        }
        Some(out)
    }
}

/// Why a missing path is not accepted as gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Doubt {
    Unreadable,
    NotADirectory,
    NoAncestor,
    MountDirectory,
    MountPoint,
    EmptyDirectory,
    HoldsMounts,
}

/// What the filesystem says about the directory an entry names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    Exists,
    /// `lstat` failed with something other than "not found".
    Unreadable,
    /// Not found, and its surroundings show the filesystem it was on.
    Gone,
    /// Not found, but possibly only because a volume is not mounted.
    Unverifiable(Doubt),
}

/// Directories whose children are conventionally separate mounts. On macOS
/// `/Volumes` is on the same device as `/`, so no `st_dev` comparison can
/// identify it; the names stay as an extra reason to keep, never to release.
fn is_mount_directory(path: &Path) -> bool {
    let s = path.to_string_lossy();
    matches!(
        s.as_ref(),
        "/Volumes" | "/mnt" | "/media" | "/net" | "/Network" | "/run/media"
    ) || path.parent() == Some(Path::new("/run/media"))
}

/// See the module documentation, rule 2.
pub fn presence(fs: &dyn Fs, path: &Path) -> Presence {
    match fs.stat(path) {
        Stat::Dir(_) | Stat::NotDir => return Presence::Exists,
        Stat::Unknown => return Presence::Unreadable,
        Stat::NotFound => {}
    }
    let mut ancestor = path.parent();
    while let Some(a) = ancestor {
        match fs.stat(a) {
            Stat::NotFound => ancestor = a.parent(),
            Stat::Unknown => return Presence::Unverifiable(Doubt::Unreadable),
            Stat::NotDir => return Presence::Unverifiable(Doubt::NotADirectory),
            Stat::Dir(device) => {
                return match doubt_about(fs, a, device) {
                    Some(doubt) => Presence::Unverifiable(doubt),
                    None => Presence::Gone,
                }
            }
        }
    }
    Presence::Unverifiable(Doubt::NoAncestor)
}

/// Whether the nearest existing ancestor could be hiding an unmounted volume.
fn doubt_about(fs: &dyn Fs, ancestor: &Path, device: u64) -> Option<Doubt> {
    if is_mount_directory(ancestor) || ancestor.parent().is_some_and(is_mount_directory) {
        return Some(Doubt::MountDirectory);
    }
    // The filesystem root is a mount point by definition.
    let Some(parent) = ancestor.parent() else {
        return Some(Doubt::MountPoint);
    };
    match fs.stat(parent) {
        Stat::Dir(parent_device) if parent_device == device => {}
        Stat::Dir(_) => return Some(Doubt::MountPoint),
        _ => return Some(Doubt::Unreadable),
    }
    match fs.children(ancestor) {
        None => Some(Doubt::Unreadable),
        // What an unmounted mount point looks like on Linux.
        Some(c) if c.count == 0 => Some(Doubt::EmptyDirectory),
        // A directory other volumes are mounted in (`/run/media/<user>`).
        Some(c) if c.dir_devices.iter().any(|d| *d != device) => Some(Doubt::HoldsMounts),
        Some(_) => None,
    }
}

/// What git knows about an entry's worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitView {
    /// The repository itself is verifiably gone; there is nobody to ask.
    RepoGone,
    /// The repository is missing, but not verifiably.
    RepoUnverifiable,
    NotListed,
    Prunable,
    /// Listed without `prunable` (also every listed worktree on git < 2.31).
    Live,
    Locked,
    /// Listing the worktrees failed or timed out.
    Failed,
}

/// Why an entry whose directory is missing was kept (or what `prune
/// --forget-unverifiable` overrode).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    Pinned,
    Exists,
    Unreadable,
    Doubt(Doubt),
    RepoUnverifiable,
    GitLive,
    GitLocked,
    GitFailed,
    PortsBusy,
}

impl Why {
    pub fn describe(&self) -> &'static str {
        match self {
            Why::Pinned => "it is pinned",
            Why::Exists => "its directory exists",
            Why::Unreadable => "its directory could not be examined",
            Why::Doubt(Doubt::Unreadable) => "a parent directory could not be examined",
            Why::Doubt(Doubt::NotADirectory) => "a parent path is no longer a directory",
            Why::Doubt(Doubt::NoAncestor) => "no parent directory exists",
            Why::Doubt(Doubt::MountDirectory) => {
                "it is below a mount directory, so its volume may only be unmounted"
            }
            Why::Doubt(Doubt::MountPoint) => {
                "the nearest existing directory is a mount point, so a volume below it may \
                 only be unmounted"
            }
            Why::Doubt(Doubt::EmptyDirectory) => {
                "the nearest existing directory is empty, as an unmounted mount point is"
            }
            Why::Doubt(Doubt::HoldsMounts) => {
                "the nearest existing directory holds other mounts, so its volume may only \
                 be unmounted"
            }
            Why::RepoUnverifiable => "its repository cannot be verified as gone",
            Why::GitLive => "git still lists the worktree",
            Why::GitLocked => "the worktree is locked in git",
            Why::GitFailed => "git could not list the project's worktrees",
            Why::PortsBusy => "its ports are in use",
        }
    }

    /// Whether `prune --forget-unverifiable` may override this reason.
    pub fn overridable(&self) -> bool {
        matches!(self, Why::Doubt(_) | Why::RepoUnverifiable | Why::GitFailed)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Release, provided the ports are unused. `forced` is the doubt that
    /// only the manual override set aside.
    Release {
        forced: Option<Why>,
    },
    Keep(Why),
}

/// The decision, ports aside. `force` is `prune --forget-unverifiable`; the
/// automatic sweep always passes `false`.
pub fn judge(pinned: bool, presence: Presence, git: GitView, force: bool) -> Verdict {
    if pinned {
        return Verdict::Keep(Why::Pinned);
    }
    let fs_doubt = match presence {
        Presence::Exists => return Verdict::Keep(Why::Exists),
        Presence::Unreadable => return Verdict::Keep(Why::Unreadable),
        Presence::Gone => None,
        Presence::Unverifiable(doubt) => Some(Why::Doubt(doubt)),
    };
    let git_doubt = match git {
        GitView::Live => return Verdict::Keep(Why::GitLive),
        GitView::Locked => return Verdict::Keep(Why::GitLocked),
        GitView::RepoGone | GitView::NotListed | GitView::Prunable => None,
        GitView::RepoUnverifiable => Some(Why::RepoUnverifiable),
        GitView::Failed => Some(Why::GitFailed),
    };
    match fs_doubt.or(git_doubt) {
        None => Verdict::Release { forced: None },
        Some(why) if force => Verdict::Release { forced: Some(why) },
        Some(why) => Verdict::Keep(why),
    }
}

/// Which projects a sweep looks at.
#[derive(Debug, Clone, Copy)]
pub enum Scope<'a> {
    All,
    Only(&'a str),
    Except(&'a str),
}

/// A project's entries that could be released (the unpinned ones).
#[derive(Debug, Clone)]
pub struct ProjectIn {
    pub key: String,
    pub entries: Vec<(String, WorktreeEntry)>,
    /// The project has no entries at all (pinned included).
    pub empty: bool,
}

/// Picks what there is to examine, from memory alone. Projects that have
/// only pinned entries are left out, so they cost nothing.
pub fn candidates(registry: &Registry, scope: Scope) -> Vec<ProjectIn> {
    registry
        .projects
        .iter()
        .filter(|(key, _)| match scope {
            Scope::All => true,
            Scope::Only(k) => key.as_str() == k,
            Scope::Except(k) => key.as_str() != k,
        })
        .map(|(key, project)| ProjectIn {
            key: key.clone(),
            entries: project
                .worktrees
                .iter()
                .filter(|(_, w)| !w.pinned)
                .map(|(p, w)| (p.clone(), w.clone()))
                .collect(),
            empty: project.worktrees.is_empty(),
        })
        .filter(|p| p.empty || !p.entries.is_empty())
        .collect()
}

/// What the evidence gathering needs from the outside world.
pub struct World<'a> {
    pub fs: &'a dyn Fs,
    /// `git worktree list` for the project with this common dir.
    pub list: &'a dyn Fn(&Path) -> Result<Vec<Listed>, String>,
    /// Called with a project's key before anything of it is touched.
    pub enter: &'a dyn Fn(&str),
}

/// One entry whose directory is not there, with the decision about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub key: String,
    pub path: String,
    /// The entry as it was when the evidence was gathered.
    pub entry: WorktreeEntry,
    pub verdict: Verdict,
}

/// The evidence, gathered without the ledger lock.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Gathered {
    pub findings: Vec<Finding>,
    /// Projects to drop once they have no entries (repository gone).
    pub droppable: Vec<String>,
    pub git_failed: Vec<(String, String)>,
    pub git_ok: Vec<String>,
}

/// Examines every candidate entry. Costs one `lstat` per entry while every
/// directory exists; ancestors, the repository and git are looked at only
/// for a project that has a missing directory.
pub fn gather(projects: &[ProjectIn], world: &World, force: bool) -> Gathered {
    let mut out = Gathered::default();
    for project in projects {
        (world.enter)(&project.key);
        let missing: Vec<(&String, &WorktreeEntry, Presence)> = project
            .entries
            .iter()
            .map(|(path, entry)| (path, entry, presence(world.fs, Path::new(path))))
            .filter(|(_, _, p)| matches!(p, Presence::Gone | Presence::Unverifiable(_)))
            .collect();
        if missing.is_empty() && !project.empty {
            continue;
        }
        let repo = presence(world.fs, Path::new(&project.key));
        if repo == Presence::Gone || (force && matches!(repo, Presence::Unverifiable(_))) {
            out.droppable.push(project.key.clone());
        }
        if missing.is_empty() {
            continue;
        }
        let listing = match repo {
            Presence::Exists => match (world.list)(Path::new(&project.key)) {
                Ok(listing) => {
                    out.git_ok.push(project.key.clone());
                    Some(listing)
                }
                Err(reason) => {
                    out.git_failed.push((project.key.clone(), reason));
                    None
                }
            },
            _ => None,
        };
        for (path, entry, here) in missing {
            let git = match (repo, &listing) {
                (Presence::Gone, _) => GitView::RepoGone,
                (Presence::Exists, Some(listing)) => view(listing, path),
                (Presence::Exists, None) => GitView::Failed,
                _ => GitView::RepoUnverifiable,
            };
            out.findings.push(Finding {
                key: project.key.clone(),
                path: path.clone(),
                entry: entry.clone(),
                verdict: judge(entry.pinned, here, git, force),
            });
        }
    }
    out
}

fn view(listing: &[Listed], path: &str) -> GitView {
    match listing.iter().find(|w| w.path.as_path() == Path::new(path)) {
        None => GitView::NotListed,
        Some(w) if w.locked => GitView::Locked,
        Some(w) if w.prunable => GitView::Prunable,
        Some(_) => GitView::Live,
    }
}

/// One thing a reclamation did or declined, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Released {
        path: String,
        block: (u16, u16),
        /// Set when only `--forget-unverifiable` allowed it.
        forced: Option<Why>,
    },
    Project {
        name: String,
        key: String,
    },
    /// An entry whose directory is missing was kept.
    Kept {
        path: String,
        block: (u16, u16),
        why: Why,
    },
}

impl Gathered {
    /// Applies the decisions to `registry`, which the caller holds the
    /// ledger lock for. Touches no file: an entry is removed only if it is
    /// still exactly the entry the evidence was gathered for (a worktree
    /// re-created in between has a fresh one) and its ports are unused now.
    pub fn apply(
        &self,
        registry: &mut Registry,
        block_free: &dyn Fn((u16, u16)) -> bool,
    ) -> Vec<Event> {
        let mut events = Vec::new();
        for finding in &self.findings {
            let block = finding.entry.block;
            let kept = |why| Event::Kept {
                path: finding.path.clone(),
                block,
                why,
            };
            let forced = match finding.verdict {
                Verdict::Keep(why) => {
                    events.push(kept(why));
                    continue;
                }
                Verdict::Release { forced } => forced,
            };
            let Some(project) = registry.projects.get_mut(&finding.key) else {
                continue;
            };
            if project.worktrees.get(&finding.path) != Some(&finding.entry) {
                continue;
            }
            // A pending block (interrupted two-phase move) is part of the
            // entry's footprint: release only when it, too, is unused.
            let unused =
                block_free(block) && finding.entry.pending_block.map(block_free).unwrap_or(true);
            if !unused {
                events.push(kept(Why::PortsBusy));
                continue;
            }
            project.worktrees.remove(&finding.path);
            events.push(Event::Released {
                path: finding.path.clone(),
                block,
                forced,
            });
        }
        for key in &self.droppable {
            if registry
                .projects
                .get(key)
                .is_some_and(|p| p.worktrees.is_empty())
            {
                if let Some(project) = registry.projects.remove(key) {
                    events.push(Event::Project {
                        name: project.name,
                        key: key.clone(),
                    });
                }
            }
        }
        events
    }
}

/// The current project's sweep, for a caller that holds the ledger lock and
/// runs in that project (`sync` slow path, `portool prune`): its files and
/// its git are the ones the command works on anyway. `list` is called only
/// when an entry's directory is missing; its failure is the caller's error.
pub fn sweep_project(
    registry: &mut Registry,
    key: &str,
    list: &dyn Fn() -> Result<Vec<Listed>, String>,
    force: bool,
) -> Result<Vec<Event>, String> {
    let projects = candidates(registry, Scope::Only(key));
    let gathered = gather(
        &projects,
        &World {
            fs: &RealFs,
            list: &|_| list(),
            enter: &|_| {},
        },
        force,
    );
    if let Some((_, reason)) = gathered.git_failed.first() {
        return Err(reason.clone());
    }
    Ok(gathered.apply(registry, &ports::block_free))
}

/// Evidence gathered before the ledger lock, to apply under it.
#[derive(Default)]
pub struct Plan {
    gathered: Gathered,
    /// Failed checks to remember, written only by [`Plan::apply`].
    memory: Option<Memory>,
}

impl Plan {
    /// Automatic use: applies silently, right before the save the command
    /// makes anyway. Never fails and never touches the ledger file. This is
    /// also the only place the failed-check memory is written: a command
    /// path that does not save the ledger never calls it, and so writes
    /// nothing at all (the price: such a path asks a failing project again).
    pub fn apply(&self, registry: &mut Registry) {
        if !self.gathered.findings.is_empty() || !self.gathered.droppable.is_empty() {
            self.gathered.apply(registry, &ports::block_free);
        }
        if let Some(memory) = &self.memory {
            memory.store();
        }
    }

    /// Manual use (`prune --all`): applies and reports.
    pub fn apply_reporting(&self, registry: &mut Registry) -> Vec<Event> {
        self.gathered.apply(registry, &ports::block_free)
    }

    /// Projects whose worktrees git could not list, with the reason.
    pub fn git_failed(&self) -> &[(String, String)] {
        &self.gathered.git_failed
    }
}

/// The automatic sweep's evidence gathering. Call it BEFORE taking the
/// ledger lock, in a command that is going to save the ledger; apply the
/// result under the lock. It never fails: any problem yields an empty plan
/// and at most one warning line.
///
/// Cost when every worktree directory exists: one read of the ledger, one
/// failed `open` of [`MEMORY_FILE`], one helper thread and one `lstat` per
/// unpinned entry. With no unpinned entry in scope: only the ledger read.
/// It writes nothing itself; what it learned about failing projects is
/// written by [`Plan::apply`].
pub fn plan(ledger: &Path, scope: Scope) -> Plan {
    let registry = match crate::store::load(ledger) {
        crate::store::LedgerLoad::Loaded(registry) => registry,
        _ => return Plan::default(),
    };
    let mut projects = candidates(&registry, scope);
    if projects.is_empty() {
        return Plan::default();
    }
    let mut memory = Memory::load(ledger);
    projects.retain(|p| !memory.remembers(&p.key));
    if projects.is_empty() {
        return Plan::default();
    }
    let mut problems: Vec<String> = Vec::new();
    let gathered = match run_bounded(projects, false, GIT_TIMEOUT, Some(BUDGET)) {
        Bounded::Done(gathered) => gathered,
        Bounded::Skipped => Gathered::default(),
        Bounded::Stuck(key) => {
            if let Some(key) = key {
                memory.note(&key);
                problems.push(format!(
                    "{}: no answer from its filesystem within {}s",
                    crate::display::text(&key),
                    BUDGET.as_secs()
                ));
            }
            Gathered::default()
        }
    };
    for (key, reason) in &gathered.git_failed {
        // A project that was merely not reached in time is neither broken
        // nor worth a warning: the next command checks it.
        if reason != OUT_OF_TIME {
            memory.note(key);
            problems.push(format!("{}: {reason}", crate::display::text(key)));
        }
    }
    if !problems.is_empty() {
        crate::warn::emit(format_args!(
            "portool: warning: could not check {} project(s) for stale allocations ({}); \
             their allocations are kept and they are checked again in {} minutes -- \
             'portool prune --all' checks them now",
            problems.len(),
            problems.join("; "),
            RETRY_AFTER.as_secs() / 60
        ));
    }
    Plan {
        gathered,
        memory: Some(memory),
    }
}

/// `portool prune --all`: the same evidence, for every project, without the
/// failure memory (the user asked now) and with patient limits. Not called
/// under the ledger lock either.
pub fn plan_manual(registry: &Registry, force: bool) -> Plan {
    let projects = candidates(registry, Scope::All);
    let gathered = match run_bounded(projects, force, MANUAL_GIT_TIMEOUT, None) {
        Bounded::Done(gathered) => gathered,
        Bounded::Skipped | Bounded::Stuck(_) => Gathered::default(),
    };
    Plan {
        gathered,
        memory: None,
    }
}

/// The reason given for a project the time budget did not reach.
const OUT_OF_TIME: &str = "no time left in this command";

enum Bounded {
    Done(Gathered),
    /// Too many earlier helpers are still blocked.
    Skipped,
    /// The budget ran out while this project was being examined.
    Stuck(Option<String>),
}

struct HelperSlot;

impl Drop for HelperSlot {
    fn drop(&mut self) {
        HELPERS.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Runs [`gather`] against the real world in a helper thread. A thread
/// blocked in `lstat` on a dead mount cannot be cancelled; it is abandoned
/// (holding nothing but its own data) and counted, so they cannot pile up
/// in a long-lived process.
fn run_bounded(
    projects: Vec<ProjectIn>,
    force: bool,
    git_timeout: Duration,
    budget: Option<Duration>,
) -> Bounded {
    if HELPERS.fetch_add(1, Ordering::SeqCst) >= MAX_HELPERS {
        HELPERS.fetch_sub(1, Ordering::SeqCst);
        return Bounded::Skipped;
    }
    let slot = HelperSlot;
    let current: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let progress = Arc::clone(&current);
    let deadline = budget.map(|b| Instant::now() + b);
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("portool-stale-check".into())
        .spawn(move || {
            let _slot = slot;
            let list = |dir: &Path| {
                let allowed = match deadline {
                    Some(d) => git_timeout.min(d.saturating_duration_since(Instant::now())),
                    None => git_timeout,
                };
                if allowed.is_zero() {
                    return Err(OUT_OF_TIME.to_string());
                }
                gitctx::worktree_listing_bounded(dir, allowed)
            };
            let enter = |key: &str| {
                if let Ok(mut current) = progress.lock() {
                    *current = Some(key.to_string());
                }
            };
            let gathered = gather(
                &projects,
                &World {
                    fs: &RealFs,
                    list: &list,
                    enter: &enter,
                },
                force,
            );
            let _ = tx.send(gathered);
        });
    if spawned.is_err() {
        return Bounded::Skipped; // the closure, and with it the slot, was dropped
    }
    let received = match budget {
        // A little past the helper's own deadline, so a git it is about to
        // kill is reaped by the helper rather than abandoned.
        Some(b) => rx.recv_timeout(b + Duration::from_millis(250)).ok(),
        None => rx.recv().ok(),
    };
    match received {
        Some(gathered) => Bounded::Done(gathered),
        None => Bounded::Stuck(current.lock().ok().and_then(|c| c.clone())),
    }
}

/// The projects whose check failed recently: `{ "<common dir>": <unix secs> }`.
struct Memory {
    path: PathBuf,
    failed: BTreeMap<String, u64>,
    dirty: bool,
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Drops entries that are [`RETRY_AFTER`] old or older, or from the future.
fn still_fresh(failed: BTreeMap<String, u64>, now: u64) -> BTreeMap<String, u64> {
    failed
        .into_iter()
        .filter(|(_, at)| *at <= now && now - *at < RETRY_AFTER.as_secs())
        .collect()
}

impl Memory {
    fn load(ledger: &Path) -> Memory {
        let path = ledger.with_file_name(MEMORY_FILE);
        let (failed, dirty) = match std::fs::read(&path) {
            Ok(bytes) => {
                let all: BTreeMap<String, u64> = serde_json::from_slice(&bytes).unwrap_or_default();
                let before = all.len();
                let fresh = still_fresh(all, unix_now());
                // An expired or unreadable file is rewritten or removed.
                let dirty = fresh.len() != before || before == 0;
                (fresh, dirty)
            }
            Err(_) => (BTreeMap::new(), false),
        };
        Memory {
            path,
            failed,
            dirty,
        }
    }

    fn remembers(&self, key: &str) -> bool {
        self.failed.contains_key(key)
    }

    fn note(&mut self, key: &str) {
        self.failed.insert(key.to_string(), unix_now());
        self.dirty = true;
    }

    /// Best effort: if the state directory cannot be written, the only
    /// cost is that the project is checked again by the next command.
    fn store(&self) {
        if !self.dirty {
            return;
        }
        if self.failed.is_empty() {
            let _ = std::fs::remove_file(&self.path);
        } else if let Ok(json) = serde_json::to_vec(&self.failed) {
            let _ = crate::store::write_atomic(&self.path, &json);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::ProjectEntry;
    use chrono::{FixedOffset, TimeZone};
    use std::cell::Cell;
    use std::collections::BTreeSet;

    /// A fake filesystem: every existing directory with its device, plus
    /// files and unreadable paths. A directory's children are derived.
    #[derive(Default)]
    struct FakeFs {
        dirs: BTreeMap<&'static str, u64>,
        files: BTreeSet<&'static str>,
        unknown: BTreeSet<&'static str>,
    }

    impl Fs for FakeFs {
        fn stat(&self, path: &Path) -> Stat {
            let p = path.to_str().unwrap();
            if self.unknown.contains(p) {
                Stat::Unknown
            } else if let Some(device) = self.dirs.get(p) {
                Stat::Dir(*device)
            } else if self.files.contains(p) {
                Stat::NotDir
            } else {
                Stat::NotFound
            }
        }
        fn children(&self, dir: &Path) -> Option<Children> {
            let is_child = |p: &str| Path::new(p).parent() == Some(dir);
            if self.unknown.iter().any(|p| is_child(p)) {
                return None;
            }
            let dir_devices: Vec<u64> = self
                .dirs
                .iter()
                .filter(|(p, _)| is_child(p))
                .map(|(_, d)| *d)
                .collect();
            Some(Children {
                count: dir_devices.len() + self.files.iter().filter(|p| is_child(p)).count(),
                dir_devices,
            })
        }
    }

    /// Directories on device 1 unless given as `("path", device)`.
    fn fs(dirs: &[&'static str]) -> FakeFs {
        FakeFs {
            dirs: dirs.iter().map(|d| (*d, 1)).collect(),
            ..Default::default()
        }
    }

    fn on(mut f: FakeFs, path: &'static str, device: u64) -> FakeFs {
        f.dirs.insert(path, device);
        f
    }

    const GONE: Presence = Presence::Gone;

    #[test]
    fn missing_path_in_a_populated_ordinary_directory_is_gone() {
        let f = fs(&["/", "/work", "/work/other"]);
        assert_eq!(presence(&f, Path::new("/work/wt")), GONE);
        assert_eq!(presence(&f, Path::new("/work/a/b/wt")), GONE);
        let mut f = fs(&["/", "/work"]);
        f.files.insert("/work/notes.txt");
        assert_eq!(presence(&f, Path::new("/work/wt")), GONE);
    }

    #[test]
    fn existing_path_of_any_kind_exists() {
        assert_eq!(
            presence(&fs(&["/", "/work", "/work/wt"]), Path::new("/work/wt")),
            Presence::Exists
        );
        let mut f = fs(&["/", "/work"]);
        f.files.insert("/work/wt"); // a file or a (dangling) symlink
        assert_eq!(presence(&f, Path::new("/work/wt")), Presence::Exists);
    }

    #[test]
    fn unreadable_path_or_ancestor_is_not_gone() {
        let mut f = fs(&["/", "/work", "/work/other"]);
        f.unknown.insert("/work/wt");
        assert_eq!(presence(&f, Path::new("/work/wt")), Presence::Unreadable);
        let mut f = fs(&["/"]);
        f.unknown.insert("/work");
        assert_eq!(
            presence(&f, Path::new("/work/a/wt")),
            Presence::Unverifiable(Doubt::Unreadable)
        );
        // A sibling that cannot be examined might be a mount.
        let mut f = fs(&["/", "/work", "/work/other"]);
        f.unknown.insert("/work/odd");
        assert_eq!(
            presence(&f, Path::new("/work/wt")),
            Presence::Unverifiable(Doubt::Unreadable)
        );
    }

    #[test]
    fn a_file_where_a_parent_was_is_not_gone() {
        let mut f = fs(&["/", "/work"]);
        f.files.insert("/work/a");
        assert_eq!(
            presence(&f, Path::new("/work/a/wt")),
            Presence::Unverifiable(Doubt::NotADirectory)
        );
    }

    #[test]
    fn empty_nearest_directory_is_not_gone() {
        // Linux: an unmounted mount point stays behind as an empty
        // directory on its parent's device -- anywhere, not just in /mnt.
        let f = fs(&["/", "/srv", "/srv/x", "/srv/data"]);
        assert_eq!(
            presence(&f, Path::new("/srv/data/wt/a")),
            Presence::Unverifiable(Doubt::EmptyDirectory)
        );
    }

    #[test]
    fn nearest_directory_that_is_a_mount_point_is_not_gone() {
        // /srv/data is a mounted volume (device 2) with content; a volume
        // mounted below it whose mount point was removed looks like this.
        let f = on(
            on(fs(&["/", "/srv", "/srv/x"]), "/srv/data", 2),
            "/srv/data/keep",
            2,
        );
        assert_eq!(
            presence(&f, Path::new("/srv/data/sub/wt")),
            Presence::Unverifiable(Doubt::MountPoint)
        );
        // The filesystem root is a mount point too.
        assert_eq!(
            presence(&fs(&["/", "/etc"]), Path::new("/wt")),
            Presence::Unverifiable(Doubt::MountPoint)
        );
    }

    #[test]
    fn mounted_volume_empty_sub_mount_point_is_not_gone() {
        // /srv/data is mounted (device 2); /srv/data/sub is the empty mount
        // point of a second volume that is not mounted.
        let f = on(
            on(
                on(fs(&["/", "/srv", "/srv/x"]), "/srv/data", 2),
                "/srv/data/proj",
                2,
            ),
            "/srv/data/sub",
            2,
        );
        assert_eq!(
            presence(&f, Path::new("/srv/data/sub/wt")),
            Presence::Unverifiable(Doubt::EmptyDirectory)
        );
    }

    #[test]
    fn missing_directory_inside_a_mounted_volume_is_gone() {
        let f = on(
            on(
                on(fs(&["/", "/srv", "/srv/x"]), "/srv/data", 2),
                "/srv/data/proj",
                2,
            ),
            "/srv/data/proj/other",
            2,
        );
        assert_eq!(presence(&f, Path::new("/srv/data/proj/wt")), GONE);
    }

    #[test]
    fn directory_holding_other_mounts_is_not_gone() {
        // /run/media/u: disk `d` was unmounted (its directory removed),
        // disk `e` (device 3) is still mounted next to where it was.
        let f = on(
            fs(&["/", "/home", "/home/u", "/home/u/mnt"]),
            "/home/u/mnt/e",
            3,
        );
        assert_eq!(
            presence(&f, Path::new("/home/u/mnt/d/wt")),
            Presence::Unverifiable(Doubt::HoldsMounts)
        );
    }

    #[test]
    fn conventional_mount_directories_are_never_proof() {
        // macOS: /Volumes is on the root device, so only its name tells.
        let f = fs(&["/", "/Volumes", "/Volumes/Other"]);
        assert_eq!(
            presence(&f, Path::new("/Volumes/X/wt")),
            Presence::Unverifiable(Doubt::MountDirectory)
        );
        let f = fs(&["/", "/mnt", "/mnt/data", "/mnt/data/x"]);
        assert_eq!(
            presence(&f, Path::new("/mnt/data/wt")),
            Presence::Unverifiable(Doubt::MountDirectory)
        );
        let f = fs(&["/", "/run", "/run/media", "/run/media/u", "/run/media/u/d"]);
        assert_eq!(
            presence(&f, Path::new("/run/media/u/d/wt")),
            Presence::Unverifiable(Doubt::MountDirectory)
        );
    }

    #[test]
    fn judge_releases_only_on_complete_evidence() {
        let release = Verdict::Release { forced: None };
        for git in [GitView::RepoGone, GitView::NotListed, GitView::Prunable] {
            assert_eq!(judge(false, GONE, git, false), release, "{git:?}");
            assert_eq!(judge(true, GONE, git, true), Verdict::Keep(Why::Pinned));
        }
        assert_eq!(
            judge(false, GONE, GitView::Live, false),
            Verdict::Keep(Why::GitLive)
        );
        assert_eq!(
            judge(false, GONE, GitView::Locked, false),
            Verdict::Keep(Why::GitLocked)
        );
        assert_eq!(
            judge(false, GONE, GitView::Failed, false),
            Verdict::Keep(Why::GitFailed)
        );
        assert_eq!(
            judge(false, GONE, GitView::RepoUnverifiable, false),
            Verdict::Keep(Why::RepoUnverifiable)
        );
        // Prunable is what git also says about an unmounted volume: the
        // filesystem doubt decides.
        let doubt = Presence::Unverifiable(Doubt::MountDirectory);
        assert_eq!(
            judge(false, doubt, GitView::Prunable, false),
            Verdict::Keep(Why::Doubt(Doubt::MountDirectory))
        );
        assert_eq!(
            judge(false, Presence::Exists, GitView::NotListed, false),
            Verdict::Keep(Why::Exists)
        );
    }

    #[test]
    fn the_override_sets_aside_doubt_but_nothing_else() {
        let doubt = Presence::Unverifiable(Doubt::EmptyDirectory);
        assert_eq!(
            judge(false, doubt, GitView::Prunable, true),
            Verdict::Release {
                forced: Some(Why::Doubt(Doubt::EmptyDirectory))
            }
        );
        assert_eq!(
            judge(false, GONE, GitView::Failed, true),
            Verdict::Release {
                forced: Some(Why::GitFailed)
            }
        );
        assert_eq!(
            judge(false, doubt, GitView::RepoUnverifiable, true),
            Verdict::Release {
                forced: Some(Why::Doubt(Doubt::EmptyDirectory))
            }
        );
        for (presence, git, why) in [
            (Presence::Exists, GitView::NotListed, Why::Exists),
            (Presence::Unreadable, GitView::NotListed, Why::Unreadable),
            (doubt, GitView::Live, Why::GitLive),
            (doubt, GitView::Locked, Why::GitLocked),
        ] {
            assert_eq!(judge(false, presence, git, true), Verdict::Keep(why));
        }
        assert_eq!(
            judge(true, doubt, GitView::Prunable, true),
            Verdict::Keep(Why::Pinned)
        );
        for why in [
            Why::Pinned,
            Why::Exists,
            Why::GitLive,
            Why::GitLocked,
            Why::PortsBusy,
        ] {
            assert!(!why.overridable(), "{why:?}");
        }
    }

    fn entry(block: (u16, u16), pinned: bool) -> WorktreeEntry {
        let now = FixedOffset::east_opt(0)
            .unwrap()
            .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
            .unwrap();
        WorktreeEntry {
            block,
            generation: 1,
            pending_block: None,
            branch: None,
            manifest_hash: None,
            pinned,
            label: None,
            allocated_at: now,
            last_seen_at: now,
        }
    }

    fn registry(projects: Vec<(&str, Vec<(&str, WorktreeEntry)>)>) -> Registry {
        let mut r = Registry::empty((3000, 9999));
        for (key, wts) in projects {
            r.projects.insert(
                key.to_string(),
                ProjectEntry {
                    name: "p".into(),
                    worktrees: wts
                        .into_iter()
                        .map(|(p, e)| (p.to_string(), e))
                        .collect::<BTreeMap<_, _>>(),
                },
            );
        }
        r
    }

    fn listed(path: &str, prunable: bool, locked: bool) -> Listed {
        Listed {
            path: PathBuf::from(path),
            prunable,
            locked,
        }
    }

    /// Gathers and applies in one go, as the real callers do in two steps.
    fn run(
        r: &mut Registry,
        f: &FakeFs,
        listing: Result<Vec<Listed>, String>,
        free: bool,
        force: bool,
    ) -> Vec<Event> {
        let list = move |_: &Path| listing.clone();
        let g = gather(
            &candidates(r, Scope::All),
            &World {
                fs: f,
                list: &list,
                enter: &|_| {},
            },
            force,
        );
        g.apply(r, &move |_| free)
    }

    /// `/r1` is a live repository; `/w` holds worktrees and one survivor.
    fn live_repo() -> FakeFs {
        fs(&["/", "/r1", "/w", "/w/other"])
    }

    fn released(path: &str, block: (u16, u16)) -> Event {
        Event::Released {
            path: path.into(),
            block,
            forced: None,
        }
    }

    #[test]
    fn worktree_git_no_longer_lists_is_released() {
        let mut r = registry(vec![("/r1", vec![("/w/gone", entry((3000, 3004), false))])]);
        let ev = run(&mut r, &live_repo(), Ok(vec![]), true, false);
        assert_eq!(ev, vec![released("/w/gone", (3000, 3004))]);
        assert!(r.projects["/r1"].worktrees.is_empty());
        assert!(r.projects.contains_key("/r1"), "a live repository stays");
    }

    #[test]
    fn worktree_removed_with_rm_and_listed_as_prunable_is_released() {
        let mut r = registry(vec![("/r1", vec![("/w/gone", entry((3000, 3004), false))])]);
        let listing = vec![listed("/r1", false, false), listed("/w/gone", true, false)];
        let ev = run(&mut r, &live_repo(), Ok(listing), true, false);
        assert_eq!(ev, vec![released("/w/gone", (3000, 3004))]);
    }

    #[test]
    fn listed_live_or_locked_worktree_is_kept() {
        for (prunable, locked, why) in [
            (false, false, Why::GitLive), // also what git < 2.31 reports
            (false, true, Why::GitLocked),
            (true, true, Why::GitLocked),
        ] {
            let mut r = registry(vec![("/r1", vec![("/w/gone", entry((3000, 3004), false))])]);
            let listing = vec![listed("/w/gone", prunable, locked)];
            let ev = run(&mut r, &live_repo(), Ok(listing), true, false);
            assert_eq!(
                ev,
                vec![Event::Kept {
                    path: "/w/gone".into(),
                    block: (3000, 3004),
                    why
                }]
            );
            assert_eq!(r.projects["/r1"].worktrees.len(), 1);
        }
    }

    #[test]
    fn prunable_worktree_on_a_possibly_unmounted_volume_is_kept() {
        let f = fs(&["/", "/r1", "/Volumes", "/Volumes/Other"]);
        let mut r = registry(vec![(
            "/r1",
            vec![("/Volumes/X/wt", entry((3000, 3004), false))],
        )]);
        let listing = vec![listed("/Volumes/X/wt", true, false)];
        let ev = run(&mut r, &f, Ok(listing), true, false);
        assert!(matches!(ev.as_slice(), [Event::Kept { .. }]), "{ev:?}");
        assert_eq!(r.projects["/r1"].worktrees.len(), 1);
    }

    #[test]
    fn gone_worktree_with_busy_ports_is_kept() {
        let mut r = registry(vec![(
            "/r1",
            vec![
                ("/w/pinned", entry((3000, 3004), true)),
                ("/w/busy", entry((3005, 3009), false)),
            ],
        )]);
        let ev = run(&mut r, &live_repo(), Ok(vec![]), false, false);
        assert_eq!(
            ev,
            vec![Event::Kept {
                path: "/w/busy".into(),
                block: (3005, 3009),
                why: Why::PortsBusy
            }]
        );
        assert_eq!(r.projects["/r1"].worktrees.len(), 2);
    }

    #[test]
    fn a_busy_pending_block_keeps_the_entry() {
        let mut e = entry((3000, 3004), false);
        e.pending_block = Some((3010, 3014));
        let mut r = registry(vec![("/r1", vec![("/w/gone", e)])]);
        let g = gather(
            &candidates(&r, Scope::All),
            &World {
                fs: &live_repo(),
                list: &|_| Ok(vec![]),
                enter: &|_| {},
            },
            false,
        );
        let ev = g.apply(&mut r, &|b| b != (3010, 3014));
        assert!(matches!(
            ev.as_slice(),
            [Event::Kept {
                why: Why::PortsBusy,
                ..
            }]
        ));
        assert_eq!(r.projects["/r1"].worktrees.len(), 1);
    }

    #[test]
    fn free_ports_alone_release_nothing() {
        // Directory present, git does not list it, ports free: kept, and
        // neither git nor the repository is even looked at.
        let f = fs(&["/", "/r1", "/w", "/w/live"]);
        let mut r = registry(vec![("/r1", vec![("/w/live", entry((3000, 3004), false))])]);
        let list = |_: &Path| -> Result<Vec<Listed>, String> { panic!("git called") };
        let g = gather(
            &candidates(&r, Scope::All),
            &World {
                fs: &f,
                list: &list,
                enter: &|_| {},
            },
            false,
        );
        assert_eq!(g, Gathered::default());
        assert!(g.apply(&mut r, &|_| true).is_empty());
        assert_eq!(r.projects["/r1"].worktrees.len(), 1);
    }

    #[test]
    fn repository_gone_but_worktree_directories_present_releases_nothing() {
        // The main clone was renamed or re-cloned; its linked worktrees
        // still exist with their .env.portool.
        let f = fs(&["/", "/w", "/w/a", "/w/b", "/elsewhere"]);
        let mut r = registry(vec![(
            "/r1/.git",
            vec![
                ("/w/a", entry((3000, 3004), false)),
                ("/w/b", entry((3005, 3009), false)),
            ],
        )]);
        let ev = run(&mut r, &f, Err("unused".into()), true, false);
        assert!(ev.is_empty(), "{ev:?}");
        assert_eq!(r.projects["/r1/.git"].worktrees.len(), 2);
    }

    #[test]
    fn repository_gone_releases_each_entry_on_its_own_evidence() {
        let f = fs(&["/", "/w", "/w/alive", "/src", "/src/keep"]);
        let mut r = registry(vec![(
            "/src/r1/.git",
            vec![
                ("/w/alive", entry((3000, 3004), false)),
                ("/w/gone", entry((3005, 3009), false)),
            ],
        )]);
        let ev = run(&mut r, &f, Err("unused".into()), true, false);
        assert_eq!(ev, vec![released("/w/gone", (3005, 3009))]);
        assert!(r.projects.contains_key("/src/r1/.git"), "an entry is left");

        // Once the last entry goes, the project goes with it.
        let f = fs(&["/", "/w", "/w/other", "/src", "/src/keep"]);
        let ev = run(&mut r, &f, Err("unused".into()), true, false);
        assert_eq!(
            ev,
            vec![
                released("/w/alive", (3000, 3004)),
                Event::Project {
                    name: "p".into(),
                    key: "/src/r1/.git".into()
                }
            ]
        );
        assert!(r.projects.is_empty());
    }

    #[test]
    fn pinned_entry_keeps_a_gone_repositorys_project() {
        let f = fs(&["/", "/w", "/w/other", "/src", "/src/keep"]);
        let mut r = registry(vec![
            ("/src/r1", vec![("/w/a", entry((3000, 3004), false))]),
            ("/src/r2", vec![("/w/b", entry((3005, 3009), true))]),
        ]);
        run(&mut r, &f, Err("unused".into()), true, false);
        assert!(!r.projects.contains_key("/src/r1"));
        assert_eq!(r.projects["/src/r2"].worktrees.len(), 1);
    }

    #[test]
    fn empty_project_of_a_gone_repository_is_dropped_and_of_a_live_one_kept() {
        let f = fs(&["/", "/live", "/w", "/src", "/src/keep"]);
        let mut r = registry(vec![("/src/gone/.git", vec![]), ("/live", vec![])]);
        let ev = run(&mut r, &f, Err("unused".into()), true, false);
        assert_eq!(
            ev,
            vec![Event::Project {
                name: "p".into(),
                key: "/src/gone/.git".into()
            }]
        );
        assert!(r.projects.contains_key("/live"));
    }

    #[test]
    fn repository_on_a_possibly_unmounted_volume_is_kept() {
        let f = fs(&["/", "/Volumes", "/Volumes/Other", "/w", "/w/other"]);
        let mut r = registry(vec![(
            "/Volumes/X/repo/.git",
            vec![("/w/gone", entry((3000, 3004), false))],
        )]);
        let ev = run(&mut r, &f, Err("unused".into()), true, false);
        assert_eq!(
            ev,
            vec![Event::Kept {
                path: "/w/gone".into(),
                block: (3000, 3004),
                why: Why::RepoUnverifiable
            }]
        );
        assert_eq!(r.projects.len(), 1);
    }

    #[test]
    fn git_failure_keeps_the_projects_entries_and_is_reported() {
        let mut r = registry(vec![("/r1", vec![("/w/gone", entry((3000, 3004), false))])]);
        let g = gather(
            &candidates(&r, Scope::All),
            &World {
                fs: &live_repo(),
                list: &|_| Err("git broke".to_string()),
                enter: &|_| {},
            },
            false,
        );
        assert_eq!(
            g.git_failed,
            vec![("/r1".to_string(), "git broke".to_string())]
        );
        let ev = g.apply(&mut r, &|_| true);
        assert!(matches!(
            ev.as_slice(),
            [Event::Kept {
                why: Why::GitFailed,
                ..
            }]
        ));
        assert_eq!(r.projects["/r1"].worktrees.len(), 1);
    }

    #[test]
    fn the_override_drops_an_entry_on_a_volume_that_never_comes_back() {
        let f = fs(&["/", "/Volumes", "/Volumes/Other"]);
        let make = || {
            registry(vec![(
                "/Volumes/X/repo/.git",
                vec![
                    ("/Volumes/X/wt", entry((3000, 3004), false)),
                    ("/Volumes/X/pinned", entry((3005, 3009), true)),
                ],
            )])
        };
        // Without the override nothing moves (this is the automatic rule).
        let mut r = make();
        let ev = run(&mut r, &f, Err("unused".into()), true, false);
        assert!(matches!(ev.as_slice(), [Event::Kept { .. }]), "{ev:?}");
        assert_eq!(r.projects["/Volumes/X/repo/.git"].worktrees.len(), 2);
        // With it, the unpinned entry goes and says why it had been kept.
        let mut r = make();
        let ev = run(&mut r, &f, Err("unused".into()), true, true);
        assert_eq!(
            ev,
            vec![Event::Released {
                path: "/Volumes/X/wt".into(),
                block: (3000, 3004),
                forced: Some(Why::Doubt(Doubt::MountDirectory))
            }]
        );
        assert_eq!(r.projects["/Volumes/X/repo/.git"].worktrees.len(), 1);
        // Busy ports are not overridable.
        let mut r = make();
        let ev = run(&mut r, &f, Err("unused".into()), false, true);
        assert!(matches!(
            ev.as_slice(),
            [Event::Kept {
                why: Why::PortsBusy,
                ..
            }]
        ));
    }

    #[test]
    fn an_entry_changed_since_the_evidence_was_gathered_is_kept() {
        let mut r = registry(vec![("/r1", vec![("/w/gone", entry((3000, 3004), false))])]);
        let g = gather(
            &candidates(&r, Scope::All),
            &World {
                fs: &live_repo(),
                list: &|_| Ok(vec![]),
                enter: &|_| {},
            },
            false,
        );
        // Another command re-created the worktree and synced it meanwhile.
        r.projects
            .get_mut("/r1")
            .unwrap()
            .worktrees
            .get_mut("/w/gone")
            .unwrap()
            .generation = 2;
        assert!(g.apply(&mut r, &|_| true).is_empty());
        // ... or pinned it.
        let mut r = registry(vec![("/r1", vec![("/w/gone", entry((3000, 3004), true))])]);
        assert!(g.apply(&mut r, &|_| true).is_empty());
        assert_eq!(r.projects["/r1"].worktrees.len(), 1);
    }

    #[test]
    fn scope_selects_projects_and_pinned_only_projects_cost_nothing() {
        let r = registry(vec![
            ("/a", vec![("/w/a", entry((3000, 3004), false))]),
            ("/b", vec![("/w/b", entry((3005, 3009), true))]),
            ("/c", vec![]),
        ]);
        let keys = |s| -> Vec<String> { candidates(&r, s).into_iter().map(|p| p.key).collect() };
        assert_eq!(keys(Scope::All), ["/a", "/c"]);
        assert_eq!(keys(Scope::Only("/a")), ["/a"]);
        assert_eq!(keys(Scope::Except("/a")), ["/c"]);
    }

    #[test]
    fn git_and_the_repository_are_examined_only_for_a_missing_directory() {
        let f = fs(&["/", "/r1", "/w", "/w/live"]);
        let r = registry(vec![("/r1", vec![("/w/live", entry((3000, 3004), false))])]);
        let entered = Cell::new(0);
        let g = gather(
            &candidates(&r, Scope::All),
            &World {
                fs: &f,
                list: &|_| panic!("git called"),
                enter: &|_| entered.set(entered.get() + 1),
            },
            false,
        );
        assert_eq!(entered.get(), 1);
        assert!(g.findings.is_empty() && g.git_ok.is_empty());
    }

    #[test]
    fn failure_memory_expires_exactly_at_the_retry_interval() {
        let limit = RETRY_AFTER.as_secs();
        let map = |at: u64| BTreeMap::from([("/r".to_string(), at)]);
        assert_eq!(still_fresh(map(1000), 1000).len(), 1);
        assert_eq!(still_fresh(map(1000), 1000 + limit - 1).len(), 1);
        assert!(still_fresh(map(1000), 1000 + limit).is_empty());
        // A stamp from the future (clock stepped back) is not trusted.
        assert!(still_fresh(map(2000), 1000).is_empty());
    }

    #[test]
    fn failure_memory_round_trips_and_removes_its_file_when_empty() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("registry.json");
        let file = dir.path().join(MEMORY_FILE);
        let mut m = Memory::load(&ledger);
        assert!(!m.remembers("/r"));
        m.store();
        assert!(!file.exists(), "nothing to remember writes nothing");
        m.note("/r");
        m.store();
        assert!(Memory::load(&ledger).remembers("/r"));
        // An expired file is removed by the next load + store.
        std::fs::write(&file, br#"{"/r": 5}"#).unwrap();
        let m = Memory::load(&ledger);
        assert!(!m.remembers("/r"));
        m.store();
        assert!(!file.exists());
    }
}
