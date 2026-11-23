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
//! 3. git agrees: the repository is itself verifiably gone, or git does not
//!    list the worktree, or lists it as `prunable` and not `locked`;
//! 4. its ports are unused right now and its directory is still not there
//!    (checked last, under the ledger lock).
//!
//! One relaxation of rule 2. When the repository is alive, git lists every
//! checkout the repository can have ([`all_checkouts_listed`]) and this
//! worktree is not among them, its registration was removed (`git worktree
//! remove`, or a prune): an unmounted volume only makes git report
//! `prunable`, it never removes the registration. Then the two doubts that
//! come from the mere shape of the surroundings -- an empty nearest
//! directory, a nearest directory that is a mount point -- are not held
//! against the entry; otherwise the last worktree removed from a worktrees
//! directory would keep its ports for ever. A worktree git has forgotten is
//! unusable until `git worktree repair` even if its files come back.
//!
//! The relaxation does not apply where "not listed" proves nothing: the
//! main checkout of a repository made with `--separate-git-dir`, reached
//! through a `.git` file, checked out as a submodule or placed by
//! `core.worktree` is never listed, so in such a repository every unlisted
//! entry may be that checkout on a volume that is away, and is kept. Nor
//! does it lift the doubts that point at a place volumes are mounted in (a
//! mount directory by name, a directory holding other mounts) or unreadable
//! surroundings: a `git worktree prune` run while a volume is unmounted
//! also removes the registration, and the worktree's `.env.portool` is back
//! when the volume is. For those entries git is not even asked
//! ([`git_can_decide`]).
//!
//! Free ports alone never release anything, and neither does a missing
//! repository: each entry needs its own directory gone.
//!
//! The sweep must not be able to stall the ledger. Everything that can block
//! on someone else's files (`lstat`, `readdir`, `git`) therefore runs
//! *before* the ledger lock is taken, in a helper thread with a time budget
//! ([`plan`], [`plan_manual`]) -- for the project the command runs in as
//! for every other. Under the lock, [`Plan::apply`] compares in-memory
//! entries, repeats the one `lstat` of each entry it is about to release (a
//! path that answered a moment ago), probes ports, and rides on the save
//! the command makes anyway.

use crate::gitctx::{self, Listed, MainConfig};
use crate::ports;
use crate::registry::{Registry, WorktreeEntry};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How long `git worktree list` for *another* project may take in the
/// automatic sweep. A healthy call takes tens of milliseconds.
pub const GIT_TIMEOUT: Duration = Duration::from_secs(2);

/// Upper bound on the time the automatic sweep adds to one command.
pub const BUDGET: Duration = Duration::from_secs(3);

/// The same bounds for `portool prune` and `prune --all`, where the user is
/// waiting for exactly this answer: per project, and in all.
pub const MANUAL_GIT_TIMEOUT: Duration = Duration::from_secs(10);
pub const MANUAL_BUDGET: Duration = Duration::from_secs(30);

/// How long past the budget the helper's last report is waited for, so a
/// git it is killing at the budget is reaped by it rather than abandoned.
const GRACE: Duration = Duration::from_millis(250);

/// A project whose check failed or timed out is not checked again by the
/// automatic sweep for this long.
pub const RETRY_AFTER: Duration = Duration::from_secs(3600);

/// Remembers failed checks, next to the ledger. Removed when its last entry
/// expires, or by the next ledger save once it is [`RETRY_AFTER`] old.
pub const MEMORY_FILE: &str = "stale-check-failed.json";

/// Helper threads that may be stuck in a blocking call at once (they cannot
/// be cancelled). Beyond this the sweep is skipped, and says so.
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
    let mount_point = match ancestor.parent() {
        // The filesystem root is a mount point by definition.
        None => true,
        Some(parent) => match fs.stat(parent) {
            Stat::Dir(parent_device) => parent_device != device,
            _ => return Some(Doubt::Unreadable),
        },
    };
    // The doubts that keep an entry whatever git says come first: the two
    // after them can be set aside by git (see `judge`).
    match fs.children(ancestor) {
        None => Some(Doubt::Unreadable),
        // A directory other volumes are mounted in (`/run/media/<user>`).
        Some(c) if c.dir_devices.iter().any(|d| *d != device) => Some(Doubt::HoldsMounts),
        Some(_) if mount_point => Some(Doubt::MountPoint),
        // What an unmounted mount point looks like on Linux.
        Some(c) if c.count == 0 => Some(Doubt::EmptyDirectory),
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
    /// Not listed, in a repository whose every checkout git lists
    /// ([`all_checkouts_listed`]): git dropped this worktree's registration.
    NotListed,
    /// Not listed, but nothing shows that the repository has no checkout
    /// git never lists -- this entry may be that checkout, alive.
    MaybeUnlistedMain,
    Prunable,
    /// Listed without `prunable` (also every listed worktree on git < 2.31).
    Live,
    Locked,
    /// Listing the worktrees failed or timed out.
    Failed,
    /// Git was not asked: no answer could have released the entry
    /// ([`git_can_decide`]).
    NotAsked,
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

/// The two doubts that rest on the shape of the surroundings alone.
fn shape_only(presence: Presence) -> bool {
    matches!(
        presence,
        Presence::Unverifiable(Doubt::EmptyDirectory | Doubt::MountPoint)
    )
}

/// Whether any answer from git could lead to this entry's release. When
/// none could, git is not spawned for it.
pub fn git_can_decide(presence: Presence, force: bool) -> bool {
    match presence {
        Presence::Exists | Presence::Unreadable => false,
        Presence::Gone => true,
        Presence::Unverifiable(_) => force || shape_only(presence),
    }
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
        // A live repository that lists all its checkouts and no longer has
        // this one outweighs a doubt that rests on the shape of the
        // directory alone.
        _ if shape_only(presence) && git == GitView::NotListed => None,
        Presence::Unverifiable(doubt) => Some(Why::Doubt(doubt)),
    };
    let git_doubt = match git {
        GitView::Live => return Verdict::Keep(Why::GitLive),
        GitView::Locked => return Verdict::Keep(Why::GitLocked),
        // Nothing was asked, so nothing may be released, forced or not.
        GitView::NotAsked => return Verdict::Keep(fs_doubt.unwrap_or(Why::GitFailed)),
        GitView::RepoGone | GitView::NotListed | GitView::MaybeUnlistedMain | GitView::Prunable => {
            None
        }
        GitView::RepoUnverifiable => Some(Why::RepoUnverifiable),
        GitView::Failed => Some(Why::GitFailed),
    };
    match fs_doubt.or(git_doubt) {
        None => Verdict::Release { forced: None },
        Some(why) if force => Verdict::Release { forced: Some(why) },
        Some(why) => Verdict::Keep(why),
    }
}

/// Whether `git worktree list` shows every checkout this repository can
/// have, so that "not listed" means "registration dropped". `key` is the
/// project's common directory.
///
/// Git lists the main checkout only for the ordinary layout (`<main>/.git`
/// is the common directory): for `git init --separate-git-dir`, a `.git`
/// file pointing elsewhere, a submodule, or `core.worktree`, the first
/// listed entry is the common directory (or the wrong directory) and the
/// real main checkout never appears. So this holds only when
/// - `core.worktree` is unset, and
/// - either the first entry is the parent of a common directory named
///   `.git` and nothing says bare (ordinary: the main checkout is listed),
/// - or the first entry is the common directory, git marks it `bare` and
///   `core.bare` is true (no main checkout exists; the `bare` mark alone is
///   not trusted, git before 2.22 printed it for any separate git dir).
///
/// Anything else, an unreadable configuration included, is `false`.
pub fn all_checkouts_listed(listing: &[Listed], key: &Path, config: Option<MainConfig>) -> bool {
    let (Some(config), Some(main)) = (config, listing.first()) else {
        return false;
    };
    if config.worktree_set {
        return false;
    }
    if main.path == key {
        return main.bare && config.bare;
    }
    !main.bare
        && !config.bare
        && key.file_name() == Some(std::ffi::OsStr::new(".git"))
        && key.parent() == Some(main.path.as_path())
}

/// Which projects a sweep looks at.
#[derive(Debug, Clone, Copy)]
pub enum Scope<'a> {
    All,
    Only(&'a str),
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

/// Why a project's git gave no answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// Git ran and failed, or could not be started.
    Git(String),
    /// Git was killed after its full time limit.
    TimedOut(Duration),
    /// The command's time budget ended before or while the project was
    /// asked: says nothing about the project.
    NotReached,
    /// This process could not set the call up: says nothing about the
    /// project either.
    Local(String),
}

impl Failure {
    pub fn describe(&self) -> String {
        match self {
            Failure::Git(reason) | Failure::Local(reason) => reason.clone(),
            Failure::TimedOut(limit) => {
                format!("git did not answer within {}s", limit.as_secs())
            }
            Failure::NotReached => "no time left in this command".to_string(),
        }
    }

    /// Whether the failure is the project's own (and worth remembering).
    fn of_the_project(&self) -> bool {
        matches!(self, Failure::Git(_) | Failure::TimedOut(_))
    }
}

/// What the evidence gathering needs from the outside world.
pub struct World<'a> {
    pub fs: &'a dyn Fs,
    /// `git worktree list` for the project with this common dir.
    pub list: &'a dyn Fn(&Path) -> Result<Vec<Listed>, Failure>,
    /// The repository's `core.bare` / `core.worktree`; `None` when unknown.
    pub config: &'a dyn Fn(&Path) -> Option<MainConfig>,
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

/// The evidence about one project.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ProjectOut {
    pub findings: Vec<Finding>,
    /// Drop the project once it has no entries (repository gone).
    pub droppable: bool,
    pub failure: Option<Failure>,
}

/// The evidence, gathered without the ledger lock.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Gathered {
    pub findings: Vec<Finding>,
    /// Projects to drop once they have no entries (repository gone).
    pub droppable: Vec<String>,
    pub failed: Vec<(String, Failure)>,
}

impl Gathered {
    fn add(&mut self, key: &str, out: ProjectOut) {
        self.findings.extend(out.findings);
        if out.droppable {
            self.droppable.push(key.to_string());
        }
        if let Some(failure) = out.failure {
            self.failed.push((key.to_string(), failure));
        }
    }
}

/// Answers each question about a path once per project: several missing
/// worktrees usually share their nearest existing directory.
struct Remembering<'a> {
    fs: &'a dyn Fs,
    stats: RefCell<BTreeMap<PathBuf, Stat>>,
    children: RefCell<BTreeMap<PathBuf, Option<Children>>>,
}

impl Fs for Remembering<'_> {
    fn stat(&self, path: &Path) -> Stat {
        if let Some(known) = self.stats.borrow().get(path) {
            return *known;
        }
        let stat = self.fs.stat(path);
        self.stats.borrow_mut().insert(path.to_path_buf(), stat);
        stat
    }
    fn children(&self, dir: &Path) -> Option<Children> {
        if let Some(known) = self.children.borrow().get(dir) {
            return known.clone();
        }
        let children = self.fs.children(dir);
        self.children
            .borrow_mut()
            .insert(dir.to_path_buf(), children.clone());
        children
    }
}

/// Examines one project's candidate entries. Costs one `lstat` per entry
/// while every directory exists. Ancestors and the repository are looked at
/// only when a directory is missing; git is asked only when its answer
/// could release one of the missing entries ([`git_can_decide`]), and its
/// configuration only when that is the last open question for an entry.
pub fn examine(project: &ProjectIn, world: &World, force: bool) -> ProjectOut {
    let fs = Remembering {
        fs: world.fs,
        stats: RefCell::default(),
        children: RefCell::default(),
    };
    let mut out = ProjectOut::default();
    let missing: Vec<(&String, &WorktreeEntry, Presence)> = project
        .entries
        .iter()
        .map(|(path, entry)| (path, entry, presence(&fs, Path::new(path))))
        .filter(|(_, _, p)| matches!(p, Presence::Gone | Presence::Unverifiable(_)))
        .collect();
    if missing.is_empty() && !project.empty {
        return out;
    }
    let key = Path::new(&project.key);
    let repo = presence(&fs, key);
    out.droppable = repo == Presence::Gone || (force && matches!(repo, Presence::Unverifiable(_)));
    if missing.is_empty() {
        return out;
    }
    let ask = repo == Presence::Exists && missing.iter().any(|(_, _, p)| git_can_decide(*p, force));
    let listing = match ask {
        true => match (world.list)(key) {
            Ok(listing) => Some(listing),
            Err(failure) => {
                out.failure = Some(failure);
                None
            }
        },
        false => None,
    };
    let mut every_checkout_listed: Option<bool> = None;
    for (path, entry, here) in missing {
        let git = match (repo, &listing) {
            (Presence::Gone, _) => GitView::RepoGone,
            (Presence::Exists, _) if !ask => GitView::NotAsked,
            (Presence::Exists, None) => GitView::Failed,
            (Presence::Exists, Some(listing)) => match view(listing, path) {
                // Only here does it matter whether "not listed" is certain.
                GitView::MaybeUnlistedMain if shape_only(here) && !force => {
                    let all = *every_checkout_listed.get_or_insert_with(|| {
                        all_checkouts_listed(listing, key, (world.config)(key))
                    });
                    match all {
                        true => GitView::NotListed,
                        false => GitView::MaybeUnlistedMain,
                    }
                }
                seen => seen,
            },
            _ => GitView::RepoUnverifiable,
        };
        out.findings.push(Finding {
            key: project.key.clone(),
            path: path.clone(),
            entry: entry.clone(),
            verdict: judge(entry.pinned, here, git, force),
        });
    }
    out
}

/// Whether two paths could name the same directory although their bytes
/// differ: letter case (a case-insensitive filesystem) and the Unicode
/// normalisation of non-ASCII names (git prints NFC on macOS where the
/// ledger may hold NFD). Deliberately generous -- components that are both
/// non-ASCII always match -- because a wrong "same" only keeps an entry,
/// while a wrong "different" could release a live one.
fn could_be_same(a: &Path, b: &Path) -> bool {
    let mut left = a.components();
    let mut right = b.components();
    loop {
        match (left.next(), right.next()) {
            (None, None) => return true,
            (Some(x), Some(y)) => {
                let (x, y) = (
                    x.as_os_str().to_string_lossy(),
                    y.as_os_str().to_string_lossy(),
                );
                let same = if x.is_ascii() || y.is_ascii() {
                    x.to_lowercase() == y.to_lowercase()
                } else {
                    true
                };
                if !same {
                    return false;
                }
            }
            _ => return false,
        }
    }
}

/// What git's listing says about the worktree at `path`: the exact entry,
/// else one that [`could_be_same`]. "Not listed" has to be certain, and
/// even then says nothing about a checkout git never lists.
fn view(listing: &[Listed], path: &str) -> GitView {
    let same = |w: &&Listed| w.path.as_path() == Path::new(path);
    let alike = |w: &&Listed| could_be_same(&w.path, Path::new(path));
    match listing.iter().find(same).or(listing.iter().find(alike)) {
        None => GitView::MaybeUnlistedMain,
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
    /// ledger lock for. An entry is removed only if it is still exactly the
    /// entry the evidence was gathered for (a worktree re-created in
    /// between has a fresh one), its directory is still not there -- one
    /// `lstat` of a path that answered a moment ago, and only for an entry
    /// about to be released: `sync`'s fast path re-creates a directory
    /// without touching its entry -- and its ports are unused now.
    pub fn apply(
        &self,
        registry: &mut Registry,
        fs: &dyn Fs,
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
            match fs.stat(Path::new(&finding.path)) {
                Stat::NotFound => {}
                Stat::Unknown => {
                    events.push(kept(Why::Unreadable));
                    continue;
                }
                Stat::Dir(_) | Stat::NotDir => continue, // it is back
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

/// Evidence gathered before the ledger lock, to apply under it.
#[derive(Default)]
pub struct Plan {
    gathered: Gathered,
    /// Failed checks to remember, written only by [`Plan::apply`].
    memory: Option<Memory>,
    /// Git's own failure for the project the command runs in.
    current_failure: Option<String>,
    /// What a manual run could not check at all.
    unchecked: Vec<String>,
}

impl Plan {
    /// Automatic use: applies silently, right before the save the command
    /// makes anyway. Never fails and never touches the ledger file. This is
    /// also the only place the failed-check memory is written: a command
    /// path that does not save the ledger never calls it, and so writes
    /// nothing at all (the price: such a path asks a failing project again).
    pub fn apply(&self, registry: &mut Registry) {
        if !self.gathered.findings.is_empty() || !self.gathered.droppable.is_empty() {
            self.gathered.apply(registry, &RealFs, &ports::block_free);
        }
        if let Some(memory) = &self.memory {
            memory.store();
        }
    }

    /// Manual use (`prune`): applies and reports.
    pub fn apply_reporting(&self, registry: &mut Registry) -> Vec<Event> {
        self.gathered.apply(registry, &RealFs, &ports::block_free)
    }

    /// Projects whose worktrees git could not list, with the reason.
    pub fn git_failed(&self) -> Vec<(&str, String)> {
        self.gathered
            .failed
            .iter()
            .filter(|(_, failure)| failure.of_the_project())
            .map(|(key, failure)| (key.as_str(), failure.describe()))
            .collect()
    }

    /// Why git could not list the worktrees of the project the command runs
    /// in (the `current` given to [`plan`]), when it ran and failed.
    pub fn current_failure(&self) -> Option<&str> {
        self.current_failure.as_deref()
    }

    /// What a manual run could not check: one line each. Not empty means
    /// the run is incomplete, which is not "nothing to prune".
    pub fn unchecked(&self) -> &[String] {
        &self.unchecked
    }
}

/// The automatic sweep's evidence gathering, for every project. Call it
/// BEFORE taking the ledger lock, in a command that is going to save the
/// ledger; apply the result under the lock. It never fails: any problem
/// yields a plan that releases less, and at most one warning line per kind
/// of problem.
///
/// `current` is the project the command itself runs in, when it works on
/// that repository anyway (`sync`): it is examined even if a failure of it
/// is remembered, and a failure of its git is not remembered or warned
/// about here but handed to the caller ([`Plan::current_failure`]).
///
/// Cost when every worktree directory exists: one read of the ledger, one
/// failed `open` of [`MEMORY_FILE`], one helper thread and one `lstat` per
/// unpinned entry. With no unpinned entry in scope: only the ledger read.
/// It writes nothing itself; what it learned about failing projects is
/// written by [`Plan::apply`].
pub fn plan(ledger: &Path, current: Option<&str>) -> Plan {
    let registry = match crate::store::load(ledger) {
        crate::store::LedgerLoad::Loaded(registry) => registry,
        _ => return Plan::default(),
    };
    let mut projects = candidates(&registry, Scope::All);
    if projects.is_empty() {
        return Plan::default();
    }
    let mut memory = Memory::load(ledger);
    projects.retain(|p| current == Some(p.key.as_str()) || !memory.remembers(&p.key));
    if projects.is_empty() {
        return Plan::default();
    }
    let outcome = run_bounded(projects, false, GIT_TIMEOUT, BUDGET, Box::new(RealFs));
    if outcome.skipped {
        crate::warn::emit(format_args!(
            "portool: warning: stale allocations were not checked: {MAX_HELPERS} earlier \
             checks in this process are still waiting for a filesystem that does not answer"
        ));
    }
    let mut remembered: Vec<String> = Vec::new();
    let mut passing: Vec<String> = Vec::new();
    let mut current_failure = None;
    if let Some(key) = &outcome.stuck {
        memory.note(key);
        remembered.push(format!(
            "{}: no answer from its filesystem within {}s",
            crate::display::text(key),
            BUDGET.as_secs()
        ));
    }
    for (key, failure) in &outcome.gathered.failed {
        let line = || format!("{}: {}", crate::display::text(key), failure.describe());
        match failure {
            // Merely not reached in time: neither broken nor worth a
            // warning; the next command checks it.
            Failure::NotReached => {}
            Failure::Local(_) => passing.push(line()),
            Failure::Git(reason) if current == Some(key.as_str()) => {
                current_failure = Some(reason.clone());
            }
            Failure::Git(_) | Failure::TimedOut(_) => {
                memory.note(key);
                remembered.push(line());
            }
        }
    }
    if !remembered.is_empty() {
        crate::warn::emit(format_args!(
            "portool: warning: could not check {} project(s) for stale allocations ({}); \
             their allocations are kept and they are checked again in {} minutes -- \
             'portool prune --all' checks them now",
            remembered.len(),
            remembered.join("; "),
            RETRY_AFTER.as_secs() / 60
        ));
    }
    if !passing.is_empty() {
        crate::warn::emit(format_args!(
            "portool: warning: stale allocations of {} project(s) were not checked ({}); \
             their allocations are kept and the next command checks them again",
            passing.len(),
            passing.join("; "),
        ));
    }
    Plan {
        gathered: outcome.gathered,
        memory: Some(memory),
        current_failure,
        unchecked: Vec::new(),
    }
}

/// `portool prune` and `prune --all`: the same evidence without the failure
/// memory (the user asked now) and with patient limits
/// ([`MANUAL_GIT_TIMEOUT`] per project, [`MANUAL_BUDGET`] in all). Not
/// called under the ledger lock either. Whatever could not be checked is in
/// [`Plan::unchecked`] and [`Plan::git_failed`]; nothing is hidden.
pub fn plan_manual(registry: &Registry, scope: Scope, force: bool) -> Plan {
    let projects = candidates(registry, scope);
    let outcome = run_bounded(
        projects,
        force,
        MANUAL_GIT_TIMEOUT,
        MANUAL_BUDGET,
        Box::new(RealFs),
    );
    let mut unchecked = Vec::new();
    if outcome.skipped {
        unchecked.push(format!(
            "any project: {MAX_HELPERS} earlier checks in this process are still waiting \
             for a filesystem that does not answer"
        ));
    }
    if let Some(key) = &outcome.stuck {
        unchecked.push(format!(
            "{}: no answer from its filesystem within {}s",
            crate::display::text(key),
            MANUAL_BUDGET.as_secs()
        ));
    }
    for (key, failure) in &outcome.gathered.failed {
        if !failure.of_the_project() {
            unchecked.push(format!(
                "{}: {}",
                crate::display::text(key),
                failure.describe()
            ));
        }
    }
    Plan {
        gathered: outcome.gathered,
        memory: None,
        current_failure: None,
        unchecked,
    }
}

/// What a bounded run brought back.
#[derive(Debug, Default)]
struct Outcome {
    /// Everything that was examined; projects the budget did not reach are
    /// in `failed` as [`Failure::NotReached`].
    gathered: Gathered,
    /// Too many earlier helpers are still blocked: nothing was examined.
    skipped: bool,
    /// The project whose own examination had not returned when the budget
    /// (and the grace after it) ended.
    stuck: Option<String>,
}

struct HelperSlot;

impl Drop for HelperSlot {
    fn drop(&mut self) {
        HELPERS.fetch_sub(1, Ordering::SeqCst);
    }
}

enum Progress {
    Entered(String),
    Done(String, ProjectOut),
}

/// Runs one bounded git call inside the command's budget. A call cut short
/// by the budget rather than by its own limit says nothing about the
/// project.
fn within<T>(
    deadline: Instant,
    git_timeout: Duration,
    call: impl FnOnce(Duration) -> Result<T, gitctx::Bounded>,
) -> Result<T, Failure> {
    let allowed = git_timeout.min(deadline.saturating_duration_since(Instant::now()));
    if allowed.is_zero() {
        return Err(Failure::NotReached);
    }
    call(allowed).map_err(|e| match e {
        gitctx::Bounded::TimedOut if allowed < git_timeout => Failure::NotReached,
        gitctx::Bounded::TimedOut => Failure::TimedOut(git_timeout),
        gitctx::Bounded::Failed(reason) => Failure::Git(reason),
        gitctx::Bounded::Local(reason) => Failure::Local(reason),
    })
}

/// Runs [`examine`] for each project against the real world in a helper
/// thread, which reports project by project. A thread blocked in `lstat` on
/// a dead mount cannot be cancelled; it is abandoned (holding nothing but
/// its own data) and counted, so they cannot pile up in a long-lived
/// process. What was examined before that is kept. No project is entered
/// after the budget has ended, so the one in flight when the wait is given
/// up -- [`GRACE`] later, git being cut off at the budget -- is one whose
/// own filesystem calls did not return.
fn run_bounded(
    projects: Vec<ProjectIn>,
    force: bool,
    git_timeout: Duration,
    budget: Duration,
    fs: Box<dyn Fs + Send>,
) -> Outcome {
    let mut outcome = Outcome::default();
    if projects.is_empty() {
        return outcome;
    }
    if HELPERS.fetch_add(1, Ordering::SeqCst) >= MAX_HELPERS {
        HELPERS.fetch_sub(1, Ordering::SeqCst);
        outcome.skipped = true;
        return outcome;
    }
    let slot = HelperSlot;
    let mut waiting: Vec<String> = projects.iter().map(|p| p.key.clone()).collect();
    let deadline = Instant::now() + budget;
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("portool-stale-check".into())
        .spawn(move || {
            let _slot = slot;
            let list = |dir: &Path| {
                within(deadline, git_timeout, |allowed| {
                    gitctx::worktree_listing_bounded(dir, allowed)
                })
            };
            let config = |dir: &Path| {
                within(deadline, git_timeout, |allowed| {
                    gitctx::main_config_bounded(dir, allowed)
                })
                .ok()
            };
            let world = World {
                fs: fs.as_ref(),
                list: &list,
                config: &config,
            };
            for project in &projects {
                if Instant::now() >= deadline
                    || tx.send(Progress::Entered(project.key.clone())).is_err()
                {
                    break;
                }
                let out = examine(project, &world, force);
                if tx.send(Progress::Done(project.key.clone(), out)).is_err() {
                    break; // nobody is waiting any more
                }
            }
        });
    if spawned.is_err() {
        // The closure, and with it the slot, was dropped.
        outcome.skipped = true;
        return outcome;
    }
    let give_up = deadline + GRACE;
    let mut in_flight: Option<String> = None;
    loop {
        match rx.recv_timeout(give_up.saturating_duration_since(Instant::now())) {
            Ok(Progress::Entered(key)) => in_flight = Some(key),
            Ok(Progress::Done(key, out)) => {
                in_flight = None;
                waiting.retain(|k| *k != key);
                outcome.gathered.add(&key, out);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                outcome.stuck = in_flight;
                break;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    for key in waiting {
        if outcome.stuck.as_deref() != Some(key.as_str()) {
            outcome.gathered.failed.push((key, Failure::NotReached));
        }
    }
    outcome
}

/// The projects whose check failed recently: `{ "<common dir>": <unix secs> }`.
struct Memory {
    path: PathBuf,
    failed: BTreeMap<String, u64>,
    /// Failures seen by this command, not yet in the file.
    noted: Vec<String>,
    /// The file holds expired or unreadable content.
    outdated: bool,
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

/// The fresh entries of the memory file, and whether the file holds
/// anything else (expired entries, unreadable content).
fn read_memory(path: &Path) -> (BTreeMap<String, u64>, bool) {
    match std::fs::read(path) {
        Ok(bytes) => {
            let all: BTreeMap<String, u64> = serde_json::from_slice(&bytes).unwrap_or_default();
            let before = all.len();
            let fresh = still_fresh(all, unix_now());
            let outdated = fresh.len() != before || before == 0;
            (fresh, outdated)
        }
        Err(_) => (BTreeMap::new(), false),
    }
}

impl Memory {
    fn load(ledger: &Path) -> Memory {
        let path = ledger.with_file_name(MEMORY_FILE);
        let (failed, outdated) = read_memory(&path);
        Memory {
            path,
            failed,
            noted: Vec::new(),
            outdated,
        }
    }

    fn remembers(&self, key: &str) -> bool {
        self.failed.contains_key(key)
    }

    fn note(&mut self, key: &str) {
        self.noted.push(key.to_string());
    }

    /// Call under the ledger lock. The file is read again and this
    /// command's notes are merged into it, so what a concurrent command
    /// noted since [`Memory::load`] (which runs before the lock) is not
    /// lost. Best effort: if the state directory cannot be written, the
    /// only cost is that the project is checked again by the next command.
    fn store(&self) {
        if self.noted.is_empty() && !self.outdated {
            return;
        }
        let (mut merged, _) = read_memory(&self.path);
        let now = unix_now();
        for key in &self.noted {
            merged.insert(key.clone(), now);
        }
        if merged.is_empty() {
            let _ = std::fs::remove_file(&self.path);
        } else if let Ok(json) = serde_json::to_vec(&merged) {
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
    use std::sync::Arc;

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
    fn an_unregistered_worktree_is_not_held_by_the_shape_of_its_surroundings() {
        let release = Verdict::Release { forced: None };
        // Removed on purpose from a live repository: an empty worktrees
        // directory, or one directly below a mount root, is no obstacle.
        for doubt in [Doubt::EmptyDirectory, Doubt::MountPoint] {
            let missing = Presence::Unverifiable(doubt);
            assert_eq!(judge(false, missing, GitView::NotListed, false), release);
            // Still registered (prunable), or nobody to ask: the doubt holds.
            for git in [GitView::Prunable, GitView::RepoGone] {
                assert_eq!(
                    judge(false, missing, git, false),
                    Verdict::Keep(Why::Doubt(doubt)),
                    "{doubt:?} {git:?}"
                );
            }
            assert_eq!(
                judge(false, missing, GitView::Failed, false),
                Verdict::Keep(Why::Doubt(doubt))
            );
            assert_eq!(
                judge(true, missing, GitView::NotListed, false),
                Verdict::Keep(Why::Pinned)
            );
        }
        // Where volumes are mounted, a prune while unmounted also removes
        // the registration: those doubts hold even when git lists nothing.
        for doubt in [
            Doubt::MountDirectory,
            Doubt::HoldsMounts,
            Doubt::Unreadable,
            Doubt::NotADirectory,
            Doubt::NoAncestor,
        ] {
            assert_eq!(
                judge(
                    false,
                    Presence::Unverifiable(doubt),
                    GitView::NotListed,
                    false
                ),
                Verdict::Keep(Why::Doubt(doubt))
            );
        }
    }

    #[test]
    fn last_worktree_of_an_emptied_directory_is_released_only_when_unregistered() {
        // `/w` held only this worktree and is empty now.
        let f = fs(&["/", "/src", "/src/r1", "/src/r1/.git", "/src/keep", "/w"]);
        let make = || {
            registry(vec![(
                "/src/r1/.git",
                vec![("/w/only", entry((3000, 3004), false))],
            )])
        };
        let mut r = make();
        let main = listed("/src/r1", false, false);
        let ev = run(&mut r, &f, Ok(vec![main.clone()]), true, false);
        assert_eq!(ev, vec![released("/w/only", (3000, 3004))]);
        // Listed as prunable (rm -rf, or an unmounted mount point): kept.
        let mut r = make();
        let ev = run(
            &mut r,
            &f,
            Ok(vec![main, listed("/w/only", true, false)]),
            true,
            false,
        );
        assert_eq!(
            ev,
            vec![Event::Kept {
                path: "/w/only".into(),
                block: (3000, 3004),
                why: Why::Doubt(Doubt::EmptyDirectory)
            }]
        );
        // Repository unreadable: nothing is known, kept.
        let mut r = make();
        let ev = run(&mut r, &f, Err("broken".into()), true, false);
        assert!(matches!(ev.as_slice(), [Event::Kept { .. }]), "{ev:?}");
        assert_eq!(r.projects["/src/r1/.git"].worktrees.len(), 1);
    }

    #[test]
    fn a_listed_path_differing_only_in_case_is_not_unlisted() {
        let listing = [
            listed("/W/Only", true, false),
            listed("/w/locked", false, true),
        ];
        assert_eq!(view(&listing, "/w/only"), GitView::Prunable);
        assert_eq!(view(&listing, "/w/LOCKED"), GitView::Locked);
        assert_eq!(view(&listing, "/w/other"), GitView::MaybeUnlistedMain);
    }

    /// The tests that fill or count the helper slots run one at a time.
    static HELPER_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Waits until every helper a test left parked has given its slot back.
    fn helpers_finish() {
        for _ in 0..500 {
            if HELPERS.load(Ordering::SeqCst) == 0 {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the released helpers never finished");
    }

    /// [`BlockedFs`] for several helpers at once.
    struct SharedBlockedFs(Arc<std::sync::Mutex<std::sync::mpsc::Receiver<()>>>);

    impl Fs for SharedBlockedFs {
        fn stat(&self, _: &Path) -> Stat {
            // Wait without holding the lock, so every helper is released.
            loop {
                match self.0.lock().unwrap().try_recv() {
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => return Stat::Unknown,
                    _ => std::thread::sleep(Duration::from_millis(5)),
                }
            }
        }
        fn children(&self, _: &Path) -> Option<Children> {
            None
        }
    }

    /// A filesystem whose every answer waits until the test lets it go.
    struct BlockedFs(std::sync::Mutex<std::sync::mpsc::Receiver<()>>);

    impl Fs for BlockedFs {
        fn stat(&self, _: &Path) -> Stat {
            let _ = self.0.lock().unwrap().recv();
            Stat::Unknown
        }
        fn children(&self, _: &Path) -> Option<Children> {
            None
        }
    }

    #[test]
    fn a_filesystem_that_does_not_answer_is_abandoned_within_the_budget() {
        let _guard = HELPER_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let (release, blocked) = std::sync::mpsc::channel::<()>();
        let r = registry(vec![(
            "/dead/repo",
            vec![("/dead/wt", entry((3000, 3004), false))],
        )]);
        let started = Instant::now();
        let out = run_bounded(
            candidates(&r, Scope::All),
            false,
            Duration::from_millis(50),
            Duration::from_millis(100),
            Box::new(BlockedFs(std::sync::Mutex::new(blocked))),
        );
        let took = started.elapsed();
        assert_eq!(out.stuck.as_deref(), Some("/dead/repo"));
        assert!(!out.skipped && out.gathered == Gathered::default());
        assert!(took < Duration::from_secs(5), "waited {took:?}");
        // The helper is still parked in its `lstat`; let it finish, and see
        // that it gives its slot back.
        assert!(HELPERS.load(Ordering::SeqCst) >= 1);
        drop(release);
        helpers_finish();
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
            bare: false,
        }
    }

    /// An ordinary repository's configuration.
    const ORDINARY: Option<MainConfig> = Some(MainConfig {
        bare: false,
        worktree_set: false,
    });

    /// [`examine`] for several projects, as the helper thread runs it.
    fn gather(projects: &[ProjectIn], world: &World, force: bool) -> Gathered {
        let mut out = Gathered::default();
        for project in projects {
            out.add(&project.key, examine(project, world, force));
        }
        out
    }

    /// Gathers and applies in one go, as the real callers do in two steps.
    /// The repositories are ordinary ones (see [`ORDINARY`]).
    fn run(
        r: &mut Registry,
        f: &FakeFs,
        listing: Result<Vec<Listed>, String>,
        free: bool,
        force: bool,
    ) -> Vec<Event> {
        let list = move |_: &Path| listing.clone().map_err(Failure::Git);
        let g = gather(
            &candidates(r, Scope::All),
            &World {
                fs: f,
                list: &list,
                config: &|_| ORDINARY,
            },
            force,
        );
        g.apply(r, f, &move |_| free)
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
                config: &|_| ORDINARY,
            },
            false,
        );
        let ev = g.apply(&mut r, &live_repo(), &|b| b != (3010, 3014));
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
        let list = |_: &Path| -> Result<Vec<Listed>, Failure> { panic!("git called") };
        let g = gather(
            &candidates(&r, Scope::All),
            &World {
                fs: &f,
                list: &list,
                config: &|_| panic!("git called"),
            },
            false,
        );
        assert_eq!(g, Gathered::default());
        assert!(g.apply(&mut r, &f, &|_| true).is_empty());
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
                list: &|_| Err(Failure::Git("git broke".to_string())),
                config: &|_| ORDINARY,
            },
            false,
        );
        assert_eq!(
            g.failed,
            vec![("/r1".to_string(), Failure::Git("git broke".to_string()))]
        );
        let ev = g.apply(&mut r, &live_repo(), &|_| true);
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
                config: &|_| ORDINARY,
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
        assert!(g.apply(&mut r, &live_repo(), &|_| true).is_empty());
        // ... or pinned it.
        let mut r = registry(vec![("/r1", vec![("/w/gone", entry((3000, 3004), true))])]);
        assert!(g.apply(&mut r, &live_repo(), &|_| true).is_empty());
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
    }

    #[test]
    fn git_and_the_repository_are_examined_only_for_a_missing_directory() {
        let f = fs(&["/", "/r1", "/w", "/w/live"]);
        let r = registry(vec![("/r1", vec![("/w/live", entry((3000, 3004), false))])]);
        let g = gather(
            &candidates(&r, Scope::All),
            &World {
                fs: &f,
                list: &|_| panic!("git called"),
                config: &|_| panic!("git called"),
            },
            false,
        );
        assert_eq!(g, Gathered::default());
    }

    /// Counts the questions a fake filesystem is asked.
    struct Counting<'a> {
        fs: &'a FakeFs,
        stats: Cell<usize>,
        reads: Cell<usize>,
    }

    impl Fs for Counting<'_> {
        fn stat(&self, path: &Path) -> Stat {
            self.stats.set(self.stats.get() + 1);
            self.fs.stat(path)
        }
        fn children(&self, dir: &Path) -> Option<Children> {
            self.reads.set(self.reads.get() + 1);
            self.fs.children(dir)
        }
    }

    fn config(bare: bool, worktree_set: bool) -> Option<MainConfig> {
        Some(MainConfig { bare, worktree_set })
    }

    #[test]
    fn not_listed_is_certain_only_where_git_lists_every_checkout() {
        let entry_at = |path: &str, bare: bool| Listed {
            bare,
            ..listed(path, false, false)
        };
        let ordinary = [entry_at("/src/r", false), entry_at("/w/x", false)];
        let key = Path::new("/src/r/.git");
        assert!(all_checkouts_listed(&ordinary, key, ORDINARY));
        // `core.worktree` moves the main checkout somewhere git never lists.
        assert!(!all_checkouts_listed(&ordinary, key, config(false, true)));
        // The configuration could not be read, or contradicts the listing.
        assert!(!all_checkouts_listed(&ordinary, key, None));
        assert!(!all_checkouts_listed(&ordinary, key, config(true, false)));
        assert!(!all_checkouts_listed(&[], key, ORDINARY));
        // `--separate-git-dir`, a `.git` file, a submodule: the first entry
        // is the common directory itself and the checkout is not listed.
        let separate = [entry_at("/gd/a.git", false), entry_at("/w/x", false)];
        let gd = Path::new("/gd/a.git");
        assert!(!all_checkouts_listed(&separate, gd, ORDINARY));
        assert!(!all_checkouts_listed(&separate, gd, config(false, true)));
        // A bare repository has no main checkout -- when git's mark and
        // the configuration agree (old git marks any separate git dir).
        let bare = [entry_at("/gd/a.git", true), entry_at("/w/x", false)];
        assert!(all_checkouts_listed(&bare, gd, config(true, false)));
        assert!(!all_checkouts_listed(&bare, gd, config(false, false)));
        assert!(!all_checkouts_listed(&bare, gd, config(true, true)));
        // A first entry that is neither: nothing is known.
        assert!(!all_checkouts_listed(
            &ordinary,
            Path::new("/else/.git"),
            ORDINARY
        ));
        assert!(!all_checkouts_listed(
            &ordinary,
            Path::new("/src/r/git"),
            ORDINARY
        ));
    }

    #[test]
    fn a_checkout_git_never_lists_is_kept_while_its_volume_is_away() {
        // `git init --separate-git-dir /gd/a.git /vol/a`: the main checkout
        // /vol/a is never in the listing. Its volume is unmounted, leaving
        // the empty mount point /vol.
        let f = fs(&["/", "/gd", "/gd/a.git", "/vol"]);
        let make = || {
            registry(vec![(
                "/gd/a.git",
                vec![("/vol/a", entry((3000, 3004), false))],
            )])
        };
        let gathered = |cfg: Option<MainConfig>, bare: bool| {
            let main = Listed {
                bare,
                ..listed("/gd/a.git", false, false)
            };
            let list = move |_: &Path| Ok(vec![main.clone()]);
            gather(
                &candidates(&make(), Scope::All),
                &World {
                    fs: &f,
                    list: &list,
                    config: &move |_| cfg,
                },
                false,
            )
        };
        let kept = vec![Event::Kept {
            path: "/vol/a".into(),
            block: (3000, 3004),
            why: Why::Doubt(Doubt::EmptyDirectory),
        }];
        for (cfg, bare) in [
            (ORDINARY, false),            // --separate-git-dir, .git file
            (config(false, true), false), // submodule
            (None, false),                // configuration unreadable
            (ORDINARY, true),             // git < 2.22 marks it bare
        ] {
            let mut r = make();
            let ev = gathered(cfg, bare).apply(&mut r, &f, &|_| true);
            assert_eq!(ev, kept, "{cfg:?} bare={bare}");
            assert_eq!(r.projects["/gd/a.git"].worktrees.len(), 1);
        }
        // A really bare repository has no such checkout: a linked worktree
        // it no longer registers is released from the emptied directory.
        let mut r = make();
        let ev = gathered(config(true, false), true).apply(&mut r, &f, &|_| true);
        assert_eq!(ev, vec![released("/vol/a", (3000, 3004))]);
    }

    #[test]
    fn a_listed_path_in_another_unicode_form_is_not_unlisted() {
        // The ledger holds NFD ("e" + combining acute), git prints NFC.
        let nfd = "/w/cafe\u{301}/wt";
        let nfc = "/w/caf\u{e9}/wt";
        assert_eq!(view(&[listed(nfc, true, false)], nfd), GitView::Prunable);
        assert_eq!(view(&[listed(nfc, false, true)], nfd), GitView::Locked);
        assert_eq!(view(&[listed(nfd, false, false)], nfc), GitView::Live);
        // An exact match wins over a merely possible one.
        let both = [listed(nfc, false, true), listed(nfd, true, false)];
        assert_eq!(view(&both, nfd), GitView::Prunable);
        // Different ASCII components, or a different depth, are different.
        assert_eq!(
            view(&[listed("/w/caf\u{e9}/other", false, false)], nfd),
            GitView::MaybeUnlistedMain
        );
        assert_eq!(
            view(&[listed("/w/caf\u{e9}", false, false)], nfd),
            GitView::MaybeUnlistedMain
        );
        assert_eq!(
            view(&[listed("/w/cafe/wt", false, false)], nfd),
            GitView::MaybeUnlistedMain
        );
    }

    #[test]
    fn a_directory_that_is_back_when_the_plan_is_applied_is_kept() {
        let mut r = registry(vec![("/r1", vec![("/w/gone", entry((3000, 3004), false))])]);
        let g = gather(
            &candidates(&r, Scope::All),
            &World {
                fs: &live_repo(),
                list: &|_| Ok(vec![]),
                config: &|_| ORDINARY,
            },
            false,
        );
        assert!(matches!(
            g.findings[0].verdict,
            Verdict::Release { forced: None }
        ));
        // Re-created between the evidence and the lock; `sync`'s fast path
        // left the entry untouched.
        let back = fs(&["/", "/r1", "/w", "/w/other", "/w/gone"]);
        assert!(g.apply(&mut r, &back, &|_| true).is_empty());
        assert_eq!(r.projects["/r1"].worktrees.len(), 1);
        // Unreadable now: kept, and said.
        let mut odd = live_repo();
        odd.unknown.insert("/w/gone");
        let ev = g.apply(&mut r, &odd, &|_| true);
        assert!(matches!(
            ev.as_slice(),
            [Event::Kept {
                why: Why::Unreadable,
                ..
            }]
        ));
        // Still gone: released; only the entry's own path was looked at.
        let f = live_repo();
        let counting = Counting {
            fs: &f,
            stats: Cell::new(0),
            reads: Cell::new(0),
        };
        let ev = g.apply(&mut r, &counting, &|_| true);
        assert_eq!(ev, vec![released("/w/gone", (3000, 3004))]);
        assert_eq!((counting.stats.get(), counting.reads.get()), (1, 0));
    }

    #[test]
    fn git_is_asked_only_when_its_answer_could_release_something() {
        let unverifiable = Presence::Unverifiable;
        assert!(git_can_decide(Presence::Gone, false));
        assert!(git_can_decide(unverifiable(Doubt::EmptyDirectory), false));
        assert!(git_can_decide(unverifiable(Doubt::MountPoint), false));
        for doubt in [
            Doubt::MountDirectory,
            Doubt::HoldsMounts,
            Doubt::Unreadable,
            Doubt::NotADirectory,
            Doubt::NoAncestor,
        ] {
            assert!(!git_can_decide(unverifiable(doubt), false), "{doubt:?}");
            // The manual override can release it, unless git lists it live.
            assert!(git_can_decide(unverifiable(doubt), true), "{doubt:?}");
            // Whatever git would have said: kept, for the doubt.
            assert_eq!(
                judge(false, unverifiable(doubt), GitView::NotAsked, false),
                Verdict::Keep(Why::Doubt(doubt))
            );
        }
        assert!(!git_can_decide(Presence::Exists, true));
        assert!(!git_can_decide(Presence::Unreadable, true));
        // Not asked never releases, even forced.
        assert_eq!(
            judge(false, Presence::Gone, GitView::NotAsked, true),
            Verdict::Keep(Why::GitFailed)
        );
    }

    #[test]
    fn an_unplugged_disk_costs_no_git_and_no_directory_listing() {
        // Two worktrees on /Volumes/Disk, which is not plugged in.
        let f = fs(&["/", "/r1", "/Volumes", "/Volumes/Other"]);
        let mut r = registry(vec![(
            "/r1",
            vec![
                ("/Volumes/Disk/a", entry((3000, 3004), false)),
                ("/Volumes/Disk/b", entry((3005, 3009), false)),
            ],
        )]);
        let counting = Counting {
            fs: &f,
            stats: Cell::new(0),
            reads: Cell::new(0),
        };
        let g = gather(
            &candidates(&r, Scope::All),
            &World {
                fs: &counting,
                list: &|_| panic!("git called"),
                config: &|_| panic!("git called"),
            },
            false,
        );
        assert_eq!(counting.reads.get(), 0, "no directory was listed");
        // a, b, /Volumes/Disk, /Volumes, the repository: each asked once.
        assert_eq!(counting.stats.get(), 5);
        let ev = g.apply(&mut r, &f, &|_| true);
        assert_eq!(ev.len(), 2);
        assert!(ev.iter().all(|e| matches!(
            e,
            Event::Kept {
                why: Why::Doubt(Doubt::MountDirectory),
                ..
            }
        )));
        // With the override git is asked: it can still say "live".
        let asked = Cell::new(0);
        let list = |_: &Path| {
            asked.set(asked.get() + 1);
            Ok(vec![listed("/Volumes/Disk/a", false, false)])
        };
        let g = gather(
            &candidates(&r, Scope::All),
            &World {
                fs: &f,
                list: &list,
                config: &|_| ORDINARY,
            },
            true,
        );
        assert_eq!(asked.get(), 1);
        let ev = g.apply(&mut r, &f, &|_| true);
        assert!(matches!(
            ev[0],
            Event::Kept {
                why: Why::GitLive,
                ..
            }
        ));
        assert!(matches!(
            ev[1],
            Event::Released {
                forced: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn the_configuration_is_read_only_when_it_is_the_last_open_question() {
        let cfg_asked = Cell::new(0);
        let cfg = |_: &Path| {
            cfg_asked.set(cfg_asked.get() + 1);
            ORDINARY
        };
        let go = |f: &FakeFs, listing: Vec<Listed>| {
            let r = registry(vec![(
                "/src/r1/.git",
                vec![
                    ("/w/a", entry((3000, 3004), false)),
                    ("/w/b", entry((3005, 3009), false)),
                ],
            )]);
            let list = move |_: &Path| Ok(listing.clone());
            gather(
                &candidates(&r, Scope::All),
                &World {
                    fs: f,
                    list: &list,
                    config: &cfg,
                },
                false,
            )
        };
        let main = listed("/src/r1", false, false);
        // Verifiably gone: "not listed" needs no certainty.
        let populated = fs(&["/", "/src", "/src/r1", "/src/r1/.git", "/w", "/w/keep"]);
        go(&populated, vec![main.clone()]);
        assert_eq!(cfg_asked.get(), 0);
        // Emptied directory, both still listed as prunable: no question.
        let emptied = fs(&["/", "/src", "/src/r1", "/src/r1/.git", "/src/x", "/w"]);
        go(
            &emptied,
            vec![
                main.clone(),
                listed("/w/a", true, false),
                listed("/w/b", true, false),
            ],
        );
        assert_eq!(cfg_asked.get(), 0);
        // Emptied directory, neither listed: asked once for both.
        let g = go(&emptied, vec![main]);
        assert_eq!(cfg_asked.get(), 1);
        assert!(g
            .findings
            .iter()
            .all(|f| f.verdict == Verdict::Release { forced: None }));
    }

    #[test]
    fn a_mount_point_that_holds_other_mounts_keeps_whatever_git_says() {
        // /srv/data (device 2) is a mount point and has /srv/data/e
        // (device 3) mounted inside it.
        let f = on(
            on(fs(&["/", "/srv", "/srv/x"]), "/srv/data", 2),
            "/srv/data/e",
            3,
        );
        assert_eq!(
            presence(&f, Path::new("/srv/data/d/wt")),
            Presence::Unverifiable(Doubt::HoldsMounts)
        );
    }

    #[test]
    fn a_git_call_cut_short_by_the_budget_says_nothing_about_the_project() {
        let t = Duration::from_secs(2);
        let now = Instant::now();
        let timed_out =
            |_: Duration| -> Result<(), gitctx::Bounded> { Err(gitctx::Bounded::TimedOut) };
        // The budget is over: not even started.
        assert_eq!(
            within(now, t, |_| -> Result<(), gitctx::Bounded> {
                panic!("called")
            }),
            Err(Failure::NotReached)
        );
        // Less than git's own limit was left, and it ran out.
        assert_eq!(
            within(now + Duration::from_secs(1), t, timed_out),
            Err(Failure::NotReached)
        );
        // Git had its full limit.
        assert_eq!(
            within(now + Duration::from_secs(60), t, timed_out),
            Err(Failure::TimedOut(t))
        );
        let local = |_: Duration| -> Result<(), gitctx::Bounded> {
            Err(gitctx::Bounded::Local("no temp".into()))
        };
        let failure = within(now + Duration::from_secs(60), t, local).unwrap_err();
        assert!(!failure.of_the_project() && !Failure::NotReached.of_the_project());
        assert!(Failure::TimedOut(t).of_the_project() && Failure::Git("x".into()).of_the_project());
    }

    /// Answers at once for everything outside `/dead`, never inside it.
    struct HalfDeadFs(std::sync::Mutex<std::sync::mpsc::Receiver<()>>, FakeFs);

    impl Fs for HalfDeadFs {
        fn stat(&self, path: &Path) -> Stat {
            if path.starts_with("/dead") {
                let _ = self.0.lock().unwrap().recv();
                return Stat::Unknown;
            }
            self.1.stat(path)
        }
        fn children(&self, dir: &Path) -> Option<Children> {
            self.1.children(dir)
        }
    }

    #[test]
    fn only_the_project_that_did_not_answer_is_blamed_and_the_rest_is_kept() {
        let _guard = HELPER_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let (release, blocked) = std::sync::mpsc::channel::<()>();
        // In key order: /a is fine, /dead/repo never answers, /z is never
        // reached. /a's repository is verifiably gone, so no git is needed.
        let r = registry(vec![
            ("/a/.git", vec![("/w/gone", entry((3000, 3004), false))]),
            ("/dead/repo", vec![("/dead/wt", entry((3005, 3009), false))]),
            ("/z/.git", vec![("/w/z", entry((3010, 3014), false))]),
        ]);
        let f = fs(&["/", "/w", "/w/other", "/x"]);
        let out = run_bounded(
            candidates(&r, Scope::All),
            false,
            Duration::from_millis(50),
            Duration::from_millis(100),
            Box::new(HalfDeadFs(std::sync::Mutex::new(blocked), f)),
        );
        assert_eq!(out.stuck.as_deref(), Some("/dead/repo"));
        assert_eq!(
            out.gathered.failed,
            vec![("/z/.git".to_string(), Failure::NotReached)]
        );
        assert_eq!(out.gathered.findings.len(), 1, "what was examined is kept");
        assert_eq!(out.gathered.findings[0].path, "/w/gone");
        drop(release);
        helpers_finish();
    }

    #[test]
    fn a_full_helper_cap_is_reported_not_passed_off_as_nothing_to_do() {
        // Occupy every slot the way stuck helpers do, for this test only.
        let (release, blocked) = std::sync::mpsc::channel::<()>();
        let blocked = Arc::new(std::sync::Mutex::new(blocked));
        let r = registry(vec![(
            "/dead/repo",
            vec![("/dead/wt", entry((3000, 3004), false))],
        )]);
        let stuck = || {
            run_bounded(
                candidates(&r, Scope::All),
                false,
                Duration::from_millis(20),
                Duration::from_millis(20),
                Box::new(SharedBlockedFs(Arc::clone(&blocked))),
            )
        };
        let _guard = HELPER_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let mut outs = Vec::new();
        while HELPERS.load(Ordering::SeqCst) < MAX_HELPERS {
            outs.push(stuck());
        }
        let out = stuck();
        assert!(out.skipped, "{out:?}");
        assert!(out.stuck.is_none() && out.gathered == Gathered::default());
        // The manual plan says what it could not check.
        let plan = plan_manual(&r, Scope::All, false);
        assert_eq!(plan.unchecked().len(), 1, "{:?}", plan.unchecked());
        assert!(plan.unchecked()[0].starts_with("any project"));
        drop(release);
        helpers_finish();
    }

    #[test]
    fn failure_memory_keeps_what_another_command_noted_meanwhile() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("registry.json");
        // Both load before either stores, as two commands do before the lock.
        let mut one = Memory::load(&ledger);
        let mut two = Memory::load(&ledger);
        one.note("/a");
        two.note("/b");
        one.store();
        two.store();
        let now = Memory::load(&ledger);
        assert!(now.remembers("/a") && now.remembers("/b"));
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
        assert!(!m.remembers("/r"), "a note counts from the next command on");
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
