//! Release of allocations whose worktree (or whole repository) is gone, for
//! every project in the ledger.
//!
//! One implementation serves both `portool prune --all` and the automatic
//! sweep that every ledger-writing command runs under the lock it already
//! holds. Deciding "gone" is deliberately conservative: only a definite
//! `NotFound` counts, and a path whose nearest existing ancestor looks like a
//! mount point (an unmounted volume) is *unknown*, never gone.

use crate::gc;
use crate::gitctx;
use crate::ports;
use crate::registry::Registry;
use std::path::{Path, PathBuf};

/// What a `stat` of a path said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stat {
    Exists,
    NotFound,
    /// Permission denied, not a directory, I/O error ...: not proof of absence.
    Unknown,
}

/// The filesystem questions the "gone" decision asks.
pub trait Fs {
    fn stat(&self, path: &Path) -> Stat;
    /// Whether `a` and `b` are on the same device (false when unknown).
    fn same_device(&self, a: &Path, b: &Path) -> bool;
}

pub struct RealFs;

impl Fs for RealFs {
    fn stat(&self, path: &Path) -> Stat {
        match std::fs::metadata(path) {
            Ok(_) => Stat::Exists,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Stat::NotFound,
            Err(_) => Stat::Unknown,
        }
    }

    fn same_device(&self, a: &Path, b: &Path) -> bool {
        use std::os::unix::fs::MetadataExt;
        match (std::fs::metadata(a), std::fs::metadata(b)) {
            (Ok(x), Ok(y)) => x.dev() == y.dev(),
            _ => false,
        }
    }
}

/// Directories whose children are conventionally separate mounts.
fn is_mount_parent(path: &Path) -> bool {
    let s = path.to_string_lossy();
    matches!(
        s.as_ref(),
        "/Volumes" | "/mnt" | "/media" | "/net" | "/Network" | "/run/media"
    ) || path.parent() == Some(Path::new("/run/media"))
}

/// True only when `path` is *known* to be absent: it is `NotFound` and its
/// nearest existing ancestor is not where an unmounted volume would have been
/// (a mount-parent directory such as `/Volumes`, or a leftover empty mount
/// point directly below one that is not a separate device).
pub fn path_gone(fs: &dyn Fs, path: &Path) -> bool {
    if fs.stat(path) != Stat::NotFound {
        return false;
    }
    let mut ancestor = path.parent();
    while let Some(a) = ancestor {
        match fs.stat(a) {
            Stat::NotFound => ancestor = a.parent(),
            Stat::Unknown => return false,
            Stat::Exists => {
                if is_mount_parent(a) {
                    return false;
                }
                if let Some(p) = a.parent() {
                    if is_mount_parent(p) && fs.same_device(a, p) {
                        return false; // an empty mount point, not mounted
                    }
                }
                return true;
            }
        }
    }
    false
}

/// One thing a reclamation removed, in the order it was removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Worktree {
        path: String,
        block: (u16, u16),
    },
    Project {
        name: String,
        key: String,
    },
    /// Listing a project's worktrees failed; its entries were kept.
    Skipped {
        key: String,
        reason: String,
    },
}

/// What the reclamation needs from the outside world.
pub struct Deps<'a> {
    pub fs: &'a dyn Fs,
    pub block_free: &'a dyn Fn((u16, u16)) -> bool,
    pub worktree_list: &'a dyn Fn(&Path) -> Result<Vec<PathBuf>, String>,
}

/// Reclaims, for every project, each unpinned entry whose worktree is gone
/// and whose ports are unused (spec §8.1/§8.2). A project whose repository
/// is gone loses all such entries and, once empty, itself. Git is consulted
/// only for projects that have at least one candidate, so the common case
/// costs one `stat` per entry.
pub fn reclaim_all_with(registry: &mut Registry, deps: &Deps) -> Vec<Event> {
    let mut events = Vec::new();
    let keys: Vec<String> = registry.projects.keys().cloned().collect();
    let mut drop_projects = Vec::new();

    for key in &keys {
        let project = registry
            .projects
            .get_mut(key)
            .expect("key came from registry.projects.keys()");

        if path_gone(deps.fs, Path::new(key)) {
            // The repository is gone. Pinned entries stay (pin contract).
            let reclaimable: Vec<String> = project
                .worktrees
                .iter()
                .filter(|(_, w)| {
                    !w.pinned
                        && (deps.block_free)(w.block)
                        && w.pending_block.map(deps.block_free).unwrap_or(true)
                })
                .map(|(p, _)| p.clone())
                .collect();
            for path in reclaimable {
                let block = project.worktrees[&path].block;
                project.worktrees.remove(&path);
                events.push(Event::Worktree { path, block });
            }
            if project.worktrees.is_empty() {
                events.push(Event::Project {
                    name: project.name.clone(),
                    key: key.clone(),
                });
                drop_projects.push(key.clone());
            }
            continue;
        }

        let has_candidate = project
            .worktrees
            .iter()
            .any(|(p, w)| !w.pinned && path_gone(deps.fs, Path::new(p)));
        if !has_candidate {
            continue;
        }
        let live = match (deps.worktree_list)(Path::new(key)) {
            Ok(live) => live,
            Err(reason) => {
                // Fail closed: a Git failure is not "zero worktrees".
                events.push(Event::Skipped {
                    key: key.clone(),
                    reason,
                });
                continue;
            }
        };
        let dir_exists = |p: &Path| !path_gone(deps.fs, p);
        for (path, block) in gc::collect(project, &live, &dir_exists, deps.block_free) {
            events.push(Event::Worktree { path, block });
        }
    }

    for key in drop_projects {
        registry.projects.remove(&key);
    }
    events
}

/// [`reclaim_all_with`] against the real filesystem, ports and git.
pub fn reclaim_all(registry: &mut Registry) -> Vec<Event> {
    let list = |dir: &Path| gitctx::worktree_list_at(dir).map_err(|e| e.to_string());
    reclaim_all_with(
        registry,
        &Deps {
            fs: &RealFs,
            block_free: &ports::block_free,
            worktree_list: &list,
        },
    )
}

/// The automatic sweep: run by every command that holds the ledger lock and
/// is about to write it. Silent (a failed git listing only keeps entries);
/// returns whether anything was removed, so the caller must save.
pub fn auto(registry: &mut Registry) -> bool {
    reclaim_all(registry)
        .iter()
        .any(|e| !matches!(e, Event::Skipped { .. }))
}

/// [`auto`], then persist at once if it removed anything. For commands that
/// would not otherwise write the ledger.
pub fn auto_and_save(registry: &mut Registry, ledger: &Path) -> crate::error::Result<()> {
    if auto(registry) {
        crate::store::save(ledger, registry)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{ProjectEntry, WorktreeEntry};
    use chrono::{FixedOffset, TimeZone};
    use std::collections::{BTreeMap, BTreeSet};

    #[derive(Default)]
    struct FakeFs {
        exists: BTreeSet<&'static str>,
        unknown: BTreeSet<&'static str>,
        /// Paths on their own device (mounted volumes).
        mounted: BTreeSet<&'static str>,
    }

    impl Fs for FakeFs {
        fn stat(&self, path: &Path) -> Stat {
            let p = path.to_str().unwrap();
            if self.unknown.contains(p) {
                Stat::Unknown
            } else if self.exists.contains(p) {
                Stat::Exists
            } else {
                Stat::NotFound
            }
        }
        fn same_device(&self, a: &Path, _b: &Path) -> bool {
            !self.mounted.contains(a.to_str().unwrap())
        }
    }

    fn fs(exists: &[&'static str]) -> FakeFs {
        FakeFs {
            exists: exists.iter().copied().collect(),
            ..Default::default()
        }
    }

    #[test]
    fn missing_path_under_an_existing_parent_is_gone() {
        assert!(path_gone(&fs(&["/", "/work"]), Path::new("/work/wt")));
    }

    #[test]
    fn missing_path_with_a_removed_parent_chain_is_gone() {
        assert!(path_gone(&fs(&["/", "/work"]), Path::new("/work/a/b/wt")));
    }

    #[test]
    fn existing_path_is_not_gone() {
        assert!(!path_gone(&fs(&["/work/wt"]), Path::new("/work/wt")));
    }

    #[test]
    fn unreadable_path_or_ancestor_is_not_gone() {
        let mut f = fs(&["/", "/work"]);
        f.unknown.insert("/work/wt");
        assert!(!path_gone(&f, Path::new("/work/wt")));
        let mut f = fs(&["/"]);
        f.unknown.insert("/work");
        assert!(!path_gone(&f, Path::new("/work/a/wt")));
    }

    #[test]
    fn unmounted_volume_is_not_gone() {
        // macOS: /Volumes exists, /Volumes/X does not.
        assert!(!path_gone(
            &fs(&["/", "/Volumes"]),
            Path::new("/Volumes/X/wt")
        ));
        // Linux: the mount point directory remains, empty, on the root device.
        assert!(!path_gone(
            &fs(&["/", "/mnt", "/mnt/data"]),
            Path::new("/mnt/data/wt/a")
        ));
        assert!(!path_gone(
            &fs(&["/", "/run", "/run/media", "/run/media/u", "/run/media/u/d"]),
            Path::new("/run/media/u/d/wt")
        ));
    }

    #[test]
    fn mounted_volume_with_a_missing_directory_is_gone() {
        let mut f = fs(&["/", "/mnt", "/mnt/data"]);
        f.mounted.insert("/mnt/data");
        assert!(path_gone(&f, Path::new("/mnt/data/wt")));
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

    fn run(r: &mut Registry, f: &FakeFs, live: Vec<&str>, free: bool) -> Vec<Event> {
        let list = move |_: &Path| Ok(live.iter().map(PathBuf::from).collect::<Vec<_>>());
        let block_free = move |_: (u16, u16)| free;
        reclaim_all_with(
            r,
            &Deps {
                fs: f,
                block_free: &block_free,
                worktree_list: &list,
            },
        )
    }

    #[test]
    fn gone_worktree_of_a_live_repository_is_released_in_any_project() {
        let f = fs(&["/", "/r1"]);
        let mut r = registry(vec![("/r1", vec![("/w/gone", entry((3000, 3004), false))])]);
        let ev = run(&mut r, &f, vec![], true);
        assert_eq!(
            ev,
            vec![Event::Worktree {
                path: "/w/gone".into(),
                block: (3000, 3004)
            }]
        );
        assert!(r.projects["/r1"].worktrees.is_empty());
    }

    #[test]
    fn worktree_still_listed_by_git_is_kept() {
        let f = fs(&["/", "/r1"]);
        let mut r = registry(vec![("/r1", vec![("/w/gone", entry((3000, 3004), false))])]);
        assert!(run(&mut r, &f, vec!["/w/gone"], true).is_empty());
    }

    #[test]
    fn pinned_and_in_use_entries_are_kept() {
        let f = fs(&["/", "/r1"]);
        let mut r = registry(vec![(
            "/r1",
            vec![
                ("/w/pinned", entry((3000, 3004), true)),
                ("/w/busy", entry((3005, 3009), false)),
            ],
        )]);
        assert!(run(&mut r, &f, vec![], false).is_empty());
        assert_eq!(r.projects["/r1"].worktrees.len(), 2);
    }

    #[test]
    fn entry_on_an_unmounted_volume_is_kept() {
        let f = fs(&["/", "/r1", "/Volumes"]);
        let mut r = registry(vec![(
            "/r1",
            vec![("/Volumes/X/wt", entry((3000, 3004), false))],
        )]);
        assert!(run(&mut r, &f, vec![], true).is_empty());
    }

    #[test]
    fn deleted_repository_drops_entries_and_the_empty_project() {
        let f = fs(&["/"]);
        let mut r = registry(vec![
            ("/r1", vec![("/w/a", entry((3000, 3004), false))]),
            ("/r2", vec![("/w/b", entry((3005, 3009), true))]),
        ]);
        let ev = run(&mut r, &f, vec![], true);
        assert!(ev.contains(&Event::Project {
            name: "p".into(),
            key: "/r1".into()
        }));
        assert!(!r.projects.contains_key("/r1"));
        // The pinned entry keeps its project alive.
        assert_eq!(r.projects["/r2"].worktrees.len(), 1);
    }

    #[test]
    fn repository_on_an_unmounted_volume_is_kept() {
        let f = fs(&["/", "/Volumes"]);
        let mut r = registry(vec![(
            "/Volumes/X/repo/.git",
            vec![("/Volumes/X/wt", entry((3000, 3004), false))],
        )]);
        assert!(run(&mut r, &f, vec![], true).is_empty());
        assert_eq!(r.projects.len(), 1);
    }

    #[test]
    fn git_failure_keeps_the_projects_entries() {
        let f = fs(&["/", "/r1"]);
        let mut r = registry(vec![("/r1", vec![("/w/gone", entry((3000, 3004), false))])]);
        let list = |_: &Path| Err("git broke".to_string());
        let ev = reclaim_all_with(
            &mut r,
            &Deps {
                fs: &f,
                block_free: &|_| true,
                worktree_list: &list,
            },
        );
        assert!(matches!(ev.as_slice(), [Event::Skipped { .. }]));
        assert_eq!(r.projects["/r1"].worktrees.len(), 1);
    }

    #[test]
    fn git_is_not_consulted_without_a_candidate() {
        let f = fs(&["/", "/r1", "/w/live"]);
        let mut r = registry(vec![("/r1", vec![("/w/live", entry((3000, 3004), false))])]);
        let list = |_: &Path| -> Result<Vec<PathBuf>, String> { panic!("git called") };
        let ev = reclaim_all_with(
            &mut r,
            &Deps {
                fs: &f,
                block_free: &|_| true,
                worktree_list: &list,
            },
        );
        assert!(ev.is_empty());
    }
}
