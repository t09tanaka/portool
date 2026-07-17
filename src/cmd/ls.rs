//! `portool ls` (spec §9.3, frozen decisions 11, 12): a human-readable
//! table by default, or a stable versioned JSON envelope with `--json`.

use crate::error::{Error, Result};
use crate::gitctx::GitCtx;
use crate::paths;
use crate::registry::{ProjectEntry, Registry, Reservation};
use crate::store;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;

/// The stable machine-readable output, versioned independently of the
/// ledger's storage schema (external review P1-7). Bump `FORMAT_VERSION`
/// on any breaking change to this shape; storage migrations (v2->v3->...)
/// must not leak here.
const FORMAT_VERSION: u32 = 1;

#[derive(Serialize)]
struct JsonOutput {
    format_version: u32,
    ok: bool,
    registry_schema_version: u32,
    effective_config: JsonConfig,
    allocations: Vec<JsonAllocation>,
    reservations: Vec<JsonReservation>,
}

#[derive(Serialize)]
struct JsonConfig {
    range: (u16, u16),
    block_align: u16,
}

#[derive(Serialize)]
struct JsonAllocation {
    project: String,
    project_key: String,
    project_id: String,
    worktree_id: String,
    path: String,
    branch: Option<String>,
    block: (u16, u16),
    generation: u64,
    pinned: bool,
    label: Option<String>,
    status: String,
    ports: Option<BTreeMap<String, u16>>,
    allocated_at: String,
    last_seen_at: String,
}

#[derive(Serialize)]
struct JsonReservation {
    block: (u16, u16),
    label: Option<String>,
    pinned: bool,
}

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

    // `Config::load()?` runs before the ledger load: a broken config is now
    // an `ls` error too (fail-closed consistency), and its real range feeds
    // the JSON envelope's `effective_config` -- fixing the old bug where a
    // missing ledger fabricated `Config::default()` instead.
    let config = crate::config::Config::load()?;

    // Read-only command: `load` never mutates the ledger. But do not lie
    // about a bad one either (batch B #10): a corrupt/unreadable ledger
    // must exit non-zero and, in JSON mode, emit an explicit error envelope
    // -- never an empty-but-valid-looking ledger that a machine consumer
    // would read as "no allocations".
    let registry = match store::load(&paths::registry_path()?) {
        store::LedgerLoad::Loaded(registry) => registry,
        store::LedgerLoad::Missing => Registry::empty(config.range),
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
                    serde_json::to_string_pretty(&serde_json::json!({
                        "format_version": FORMAT_VERSION,
                        "ok": false,
                        "error": message,
                    }))
                    .expect("error envelope always serializes")
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
        print_json(&registry, &filtered, &config);
    } else {
        print_table(&filtered, &registry.reservations);
    }
    Ok(())
}

fn print_json(
    registry: &Registry,
    filtered: &BTreeMap<String, ProjectEntry>,
    config: &crate::config::Config,
) {
    let out = JsonOutput {
        format_version: FORMAT_VERSION,
        ok: true,
        registry_schema_version: registry.version,
        effective_config: JsonConfig {
            range: config.range,
            block_align: config.block_align,
        },
        allocations: build_allocations(filtered),
        reservations: registry
            .reservations
            .iter()
            .map(|r| JsonReservation {
                block: r.block,
                label: r.label.clone(),
                pinned: r.pinned,
            })
            .collect(),
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&out).expect("JsonOutput always serializes")
    );
}

/// Builds the `allocations` array from the (already `--all`-filtered)
/// projects, deriving `status` and `ports` the same way the table and the
/// injected environment would.
fn build_allocations(filtered: &BTreeMap<String, ProjectEntry>) -> Vec<JsonAllocation> {
    let mut out = Vec::new();
    for (project_key, project) in filtered {
        for (path, w) in &project.worktrees {
            let status = if w.pinned {
                "pinned"
            } else if Path::new(path).exists() {
                "active"
            } else {
                "stale?"
            };
            out.push(JsonAllocation {
                project: project.name.clone(),
                project_key: project_key.clone(),
                project_id: crate::identity::project_id(Path::new(project_key)),
                worktree_id: crate::identity::worktree_id(Path::new(project_key), Path::new(path)),
                path: path.clone(),
                branch: w.branch.clone(),
                block: w.block,
                generation: w.generation,
                pinned: w.pinned,
                label: w.label.clone(),
                status: status.to_string(),
                ports: ports_for(Path::new(path), w.block),
                allocated_at: w.allocated_at.to_rfc3339(),
                last_seen_at: w.last_seen_at.to_rfc3339(),
            });
        }
    }
    out
}

/// The env-name -> port map this worktree's environment would receive, or
/// `None` when it cannot be derived (directory gone, manifest unreadable or
/// invalid). Mirrors `envfile::variables` minus the identity entries.
fn ports_for(worktree: &Path, block: (u16, u16)) -> Option<BTreeMap<String, u16>> {
    if !worktree.exists() {
        return None;
    }
    let manifest_path = worktree.join(".portool.toml");
    let manifest = match std::fs::read_to_string(&manifest_path) {
        Ok(text) => Some(crate::manifest::Manifest::parse(&text).ok()?),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return None,
    };
    let vars = crate::envfile::variables(block, manifest.as_ref(), "", "").ok()?;
    Some(
        vars.into_iter()
            .filter(|(name, _)| !name.starts_with("PORTOOL_"))
            .filter_map(|(name, value)| value.parse::<u16>().ok().map(|p| (name, p)))
            .collect(),
    )
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
/// included). Reservations are global, so they're printed (Task 9) after
/// the table whenever any exist, regardless of the `--all` filter.
fn print_table(projects: &BTreeMap<String, ProjectEntry>, reservations: &[Reservation]) {
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
                project: crate::display::sanitize(&project.name),
                worktree: crate::display::sanitize(&shorten_home(path, home.as_deref())),
                branch: crate::display::sanitize(worktree.branch.as_deref().unwrap_or("-")),
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

    if !reservations.is_empty() {
        println!();
        for r in reservations {
            println!(
                "reserved {}-{}  {}",
                r.block.0,
                r.block.1,
                crate::display::sanitize(r.label.as_deref().unwrap_or("-"))
            );
        }
    }
}

fn column_width(rows: &[Row], header: &Row, get: impl Fn(&Row) -> &String) -> usize {
    rows.iter()
        .map(|r| crate::display::width(get(r)))
        .chain(std::iter::once(crate::display::width(get(header))))
        .max()
        .unwrap_or(0)
}

fn print_row(row: &Row, w_project: usize, w_worktree: usize, w_branch: usize, w_block: usize) {
    use crate::display::pad;
    println!(
        "{}  {}  {}  {}  {}",
        pad(&row.project, w_project),
        pad(&row.worktree, w_worktree),
        pad(&row.branch, w_branch),
        pad(&row.block, w_block),
        row.status,
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
