//! `portool ls` (spec §9.3, frozen decisions 11, 12): a human-readable
//! table by default, or the ledger's own JSON shape with `--json`.

use crate::error::{Error, Result};
use crate::gitctx::GitCtx;
use crate::paths;
use crate::registry::{ProjectEntry, Registry};
use crate::store;
use std::collections::BTreeMap;
use std::path::Path;

/// Runs `portool ls`. Outside a git repository, `--all` is required (spec
/// frozen decision 12); without it, this is a general error (exit 1).
pub fn run(json: bool, all: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let ctx_result = GitCtx::discover(&cwd);

    let current_key = ctx_result
        .as_ref()
        .ok()
        .map(|ctx| ctx.common_dir.to_string_lossy().into_owned());

    if !all {
        ctx_result?;
    }

    // Read-only command: `load` never mutates the ledger. But do not lie
    // about a bad one either (batch B #10): a corrupt/unreadable ledger
    // must exit non-zero and, in JSON mode, emit an explicit error object
    // -- never an empty-but-valid-looking ledger that a machine consumer
    // would read as "no allocations".
    let registry = match store::load(&paths::registry_path()?) {
        store::LedgerLoad::Loaded(registry) => registry,
        store::LedgerLoad::Missing => Registry::empty(crate::config::Config::default().range),
        bad => {
            let message = match bad {
                store::LedgerLoad::Corrupt { reason } => {
                    format!("registry is corrupt: {reason}")
                }
                store::LedgerLoad::UnsupportedVersion { found, supported } => format!(
                    "registry uses unsupported schema version {found} \
                     (this build understands version {supported})"
                ),
                store::LedgerLoad::ReadError { reason } => {
                    format!("registry is unreadable: {reason}")
                }
                store::LedgerLoad::Missing | store::LedgerLoad::Loaded(_) => unreachable!(),
            };
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({ "error": message }))
                        .expect("error object always serializes")
                );
            }
            return Err(Error::General(message));
        }
    };

    let filtered: BTreeMap<String, ProjectEntry> = if all {
        registry.projects.clone()
    } else {
        let key = current_key.expect("checked above: !all requires a resolved GitCtx");
        let mut filtered = BTreeMap::new();
        if let Some(project) = registry.projects.get(&key) {
            filtered.insert(key, project.clone());
        }
        filtered
    };

    if json {
        print_json(&registry, filtered);
    } else {
        print_table(&filtered);
    }
    Ok(())
}

fn print_json(registry: &Registry, filtered: BTreeMap<String, ProjectEntry>) {
    let out = Registry {
        version: registry.version,
        range: registry.range,
        projects: filtered,
        reservations: registry.reservations.clone(),
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&out).expect("Registry always serializes")
    );
}

struct Row {
    project: String,
    worktree: String,
    branch: String,
    block: String,
    status: String,
}

/// Frozen decision 11: `PROJECT WORKTREE BRANCH BLOCK STATUS`, two spaces
/// between columns, columns left-justified to the widest cell (header
/// included).
fn print_table(projects: &BTreeMap<String, ProjectEntry>) {
    let home = std::env::var("HOME").ok().filter(|h| !h.is_empty());

    let mut rows: Vec<Row> = Vec::new();
    for project in projects.values() {
        for (path, worktree) in &project.worktrees {
            let status = if worktree.pinned {
                "pinned"
            } else if Path::new(path).exists() {
                "active"
            } else {
                "stale?"
            };
            rows.push(Row {
                project: project.name.clone(),
                worktree: shorten_home(path, home.as_deref()),
                branch: worktree.branch.clone().unwrap_or_else(|| "-".to_string()),
                block: format!("{}-{}", worktree.block.0, worktree.block.1),
                status: status.to_string(),
            });
        }
    }
    rows.sort_by(|a, b| (&a.project, &a.worktree).cmp(&(&b.project, &b.worktree)));

    let header = Row {
        project: "PROJECT".to_string(),
        worktree: "WORKTREE".to_string(),
        branch: "BRANCH".to_string(),
        block: "BLOCK".to_string(),
        status: "STATUS".to_string(),
    };

    let w_project = column_width(&rows, &header, |r| &r.project);
    let w_worktree = column_width(&rows, &header, |r| &r.worktree);
    let w_branch = column_width(&rows, &header, |r| &r.branch);
    let w_block = column_width(&rows, &header, |r| &r.block);

    print_row(&header, w_project, w_worktree, w_branch, w_block);
    for row in &rows {
        print_row(row, w_project, w_worktree, w_branch, w_block);
    }
}

fn column_width(rows: &[Row], header: &Row, get: impl Fn(&Row) -> &String) -> usize {
    rows.iter()
        .map(|r| get(r).len())
        .chain(std::iter::once(get(header).len()))
        .max()
        .unwrap_or(0)
}

fn print_row(row: &Row, w_project: usize, w_worktree: usize, w_branch: usize, w_block: usize) {
    println!(
        "{:<w_project$}  {:<w_worktree$}  {:<w_branch$}  {:<w_block$}  {}",
        row.project,
        row.worktree,
        row.branch,
        row.block,
        row.status,
        w_project = w_project,
        w_worktree = w_worktree,
        w_branch = w_branch,
        w_block = w_block,
    );
}

fn shorten_home(path: &str, home: Option<&str>) -> String {
    if let Some(home) = home {
        if let Some(rest) = path.strip_prefix(home) {
            if rest.is_empty() {
                return "~".to_string();
            }
            if let Some(rest) = rest.strip_prefix('/') {
                return format!("~/{rest}");
            }
        }
    }
    path.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shorten_home_replaces_prefix() {
        assert_eq!(
            shorten_home("/home/user/dev/myapp", Some("/home/user")),
            "~/dev/myapp"
        );
        assert_eq!(shorten_home("/home/user", Some("/home/user")), "~");
        assert_eq!(
            shorten_home("/srv/repos/myapp", Some("/home/user")),
            "/srv/repos/myapp"
        );
    }

    #[test]
    fn shorten_home_leaves_path_untouched_without_home() {
        assert_eq!(shorten_home("/srv/repos/myapp", None), "/srv/repos/myapp");
    }
}
