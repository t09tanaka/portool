//! `portool exec` (spec v0.4 §5-§10): syncs the worktree's port
//! allocation, composes an environment from env files, the parent
//! environment, and portool-managed variables — in that precedence order
//! (§6) — expands `${NAME}` references (§7), then replaces this process
//! with the requested command (§9) so signals and exit codes pass through
//! naturally.

use crate::cmd::sync;
use crate::envfile;
use crate::envread::{self, Entry, FileValue};
use crate::error::{Error, Result};
use crate::gitctx::GitCtx;
use crate::identity;
use crate::ports;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::PathBuf;

/// Runs `portool exec`. On success this never returns: the process image
/// is replaced by `command`. Any failure — sync, env construction, or the
/// exec itself — happens before the child runs (spec §5, §10).
///
/// The execution-boundary bind check (batch C #1) is opt-in as of v0.8.0
/// (external review 3rd round P1-2): `check_ports` turns it on as an
/// advisory warning; `strict` turns a detected conflict into a hard
/// failure; `reallocate_on_conflict` moves the worktree to a fresh block
/// instead of warning. `strict` and `reallocate_on_conflict` each imply the
/// check even without `check_ports`. All three default off, since a
/// worktree's own already-running dev servers legitimately occupy its
/// block.
///
/// `env_file_overrides` (v0.8.0, external review): by default the parent
/// environment beats env files (spec §6); this flag lets explicitly passed
/// env files win over inherited shell state instead, since a stale parent
/// variable can otherwise silently shadow a value the caller just set.
pub fn run(
    env_files: &[PathBuf],
    command: &[OsString],
    check_ports: bool,
    strict: bool,
    reallocate_on_conflict: bool,
    env_file_overrides: bool,
) -> Result<()> {
    // clap marks the trailing command as required, but this function is
    // also reachable programmatically -- an empty command must be a clean
    // error, not an index panic below.
    if command.is_empty() {
        return Err(Error::General(
            "portool exec: no command given (expected 'portool exec -- <COMMAND>')".to_string(),
        ));
    }

    let cwd = std::env::current_dir()?;
    let ctx = GitCtx::discover(&cwd)?;

    // Spec §5 step 3: sync first; if it fails the child is never started.
    let outcome = sync::ensure(&ctx, true)?;

    // v0.8.0 (external review 3rd round P1-2): the bind check is opt-in. A
    // worktree's own dev servers legitimately occupy the block, so a
    // default-on check made every second `portool exec` noisy. --strict
    // and --reallocate-on-conflict imply the check.
    let check = check_ports || strict || reallocate_on_conflict;
    let outcome = if !check || ports::block_free(outcome.block) {
        outcome
    } else if reallocate_on_conflict {
        eprintln!(
            "portool: ports {}-{} are in use; moving to a fresh block -- processes \
             already running keep the old ports, so this worktree may end up split \
             across old and new blocks until they are restarted",
            outcome.block.0, outcome.block.1
        );
        sync::reallocate(&ctx, true)?
    } else {
        eprintln!(
            "portool: ports {}-{} are in use -- this may be this worktree's own \
             running processes (pass --strict to fail instead)",
            outcome.block.0, outcome.block.1
        );
        if strict {
            return Err(Error::General(format!(
                "ports {}-{} are in use and --strict was given",
                outcome.block.0, outcome.block.1
            )));
        }
        outcome
    };

    let portool_vars = envfile::variables(
        outcome.block,
        outcome.manifest.as_ref(),
        &identity::project_id(&ctx.common_dir),
        &identity::worktree_id(&ctx.common_dir, &ctx.worktree_root),
    )?;

    // Spec §8: files load in the order given; relative paths stay
    // relative to the cwd exec was invoked from.
    let mut file_entries = Vec::with_capacity(env_files.len());
    for path in env_files {
        file_entries.push(envread::parse_env_file(path)?);
    }

    // Every parent variable with a UTF-8 name takes part in precedence, so
    // an env-file entry can never shadow an inherited variable (§6). A
    // non-UTF-8 *value* is lossily decoded for `${NAME}` expansion only —
    // the child still inherits its exact bytes, because parent-sourced
    // winners are never re-set on the child (see `compose`).
    let parent: Vec<(String, String)> = std::env::vars_os()
        .filter_map(|(name, value)| {
            let name = name.into_string().ok()?;
            let value = match value.into_string() {
                Ok(value) => value,
                Err(raw) => raw.to_string_lossy().into_owned(),
            };
            Some((name, value))
        })
        .collect();

    let (map, to_set) = compose(file_entries, parent, portool_vars, env_file_overrides);
    let mut resolved = envread::resolve(&map)?;
    resolved.retain(|name, _| to_set.contains(name));

    exec_command(command, &resolved)
}

/// Merges the three variable sources into one map. Default precedence
/// (spec §6): earlier env files < later env files < parent environment <
/// portool-managed variables. With `env_file_overrides` (v0.8.0), the
/// parent drops below the files: parent < files < portool, so an
/// explicitly passed env file wins over inherited shell state.
///
/// Also returns the set of names to actually set on the child: file and
/// portool winners only. Parent-sourced winners are excluded — the child
/// inherits them as-is, which keeps non-UTF-8 parent values byte-exact.
fn compose(
    file_entries: Vec<Vec<(String, FileValue)>>,
    parent: Vec<(String, String)>,
    portool_vars: Vec<(String, String)>,
    env_file_overrides: bool,
) -> (BTreeMap<String, Entry>, BTreeSet<String>) {
    let mut map = BTreeMap::new();
    let mut to_set = BTreeSet::new();
    if env_file_overrides {
        for (name, value) in parent {
            map.insert(name, Entry::Literal(value));
        }
        for entries in file_entries {
            for (name, value) in entries {
                to_set.insert(name.clone());
                map.insert(name, Entry::File(value));
            }
        }
    } else {
        for entries in file_entries {
            for (name, value) in entries {
                to_set.insert(name.clone());
                map.insert(name, Entry::File(value));
            }
        }
        for (name, value) in parent {
            to_set.remove(&name);
            map.insert(name, Entry::Literal(value));
        }
    }
    for (name, value) in portool_vars {
        to_set.insert(name.clone());
        map.insert(name, Entry::Literal(value));
    }
    (map, to_set)
}

/// Spec §9: no shell, cwd unchanged, stdio inherited. `exec` replaces the
/// process on success so signals reach the child directly; if it returns,
/// the launch failed and we map the error to exit code 127 (not found) or
/// 126 (not executable).
fn exec_command(command: &[OsString], env: &BTreeMap<String, String>) -> Result<()> {
    use std::os::unix::process::CommandExt;

    let mut cmd = std::process::Command::new(&command[0]);
    // No env_clear(): the parent environment is inherited wholesale;
    // .envs() only overlays the file/portool winners.
    cmd.args(&command[1..]).envs(env);

    let err = cmd.exec();
    let name = command[0].to_string_lossy().into_owned();
    if err.kind() == std::io::ErrorKind::NotFound {
        Err(Error::CommandNotFound(name))
    } else {
        Err(Error::CommandNotExecutable(name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envread::Quoting;

    fn file_value(raw: &str) -> FileValue {
        FileValue {
            raw: raw.to_string(),
            quoting: Quoting::None,
            origin: "test.env:1".to_string(),
        }
    }

    fn raw_of(entry: &Entry) -> &str {
        match entry {
            Entry::File(fv) => &fv.raw,
            Entry::Literal(s) => s,
        }
    }

    /// Spec §6: earlier file < later file (and later lines win within a
    /// file) < parent < portool.
    #[test]
    fn compose_applies_precedence_order() {
        let (map, to_set) = compose(
            vec![
                vec![
                    ("A".into(), file_value("file1-first")),
                    ("A".into(), file_value("file1-second")),
                    ("B".into(), file_value("file1")),
                    ("C".into(), file_value("file1")),
                    ("D".into(), file_value("file1")),
                ],
                vec![("B".into(), file_value("file2"))],
            ],
            vec![("C".into(), "parent".into()), ("D".into(), "parent".into())],
            vec![("D".into(), "portool".into())],
            false,
        );

        assert_eq!(raw_of(&map["A"]), "file1-second");
        assert_eq!(raw_of(&map["B"]), "file2");
        assert_eq!(raw_of(&map["C"]), "parent");
        assert_eq!(raw_of(&map["D"]), "portool");
        assert!(matches!(map["A"], Entry::File(_)));
        assert!(matches!(map["C"], Entry::Literal(_)));

        // File and portool winners are set on the child; parent winners
        // are inherited instead, never re-set.
        let names: Vec<&str> = to_set.iter().map(String::as_str).collect();
        assert_eq!(names, ["A", "B", "D"]);
    }

    /// Parent and portool values enter as literals, so file-side
    /// expansion can reference them while their own `$` content stays
    /// verbatim.
    #[test]
    fn compose_result_resolves_with_portool_value() {
        let (map, _) = compose(
            vec![vec![
                ("TEST_DB_PORT".into(), file_value("1111")),
                ("URL".into(), file_value("localhost:${TEST_DB_PORT}")),
            ]],
            vec![],
            vec![("TEST_DB_PORT".into(), "3005".into())],
            false,
        );
        let resolved = envread::resolve(&map).unwrap();
        assert_eq!(resolved["URL"], "localhost:3005");
        assert_eq!(resolved["TEST_DB_PORT"], "3005");
    }

    /// --env-file-overrides: parent < files < portool (external review: an
    /// explicitly passed .env.test must beat a stale parent DATABASE_URL).
    #[test]
    fn compose_with_overrides_lets_files_beat_the_parent() {
        let (map, to_set) = compose(
            vec![vec![("C".into(), file_value("file"))]],
            vec![("C".into(), "parent".into()), ("P".into(), "parent".into())],
            vec![("D".into(), "portool".into())],
            true,
        );
        assert_eq!(raw_of(&map["C"]), "file", "file wins over parent");
        assert_eq!(
            raw_of(&map["P"]),
            "parent",
            "parent-only vars still visible"
        );
        assert_eq!(raw_of(&map["D"]), "portool");
        let names: Vec<&str> = to_set.iter().map(String::as_str).collect();
        assert_eq!(
            names,
            ["C", "D"],
            "parent-only vars are inherited, not re-set"
        );
    }

    /// Portool variables stay on top regardless of the flag.
    #[test]
    fn compose_with_overrides_keeps_portool_on_top() {
        let (map, _) = compose(
            vec![vec![("D".into(), file_value("file"))]],
            vec![("D".into(), "parent".into())],
            vec![("D".into(), "portool".into())],
            true,
        );
        assert_eq!(raw_of(&map["D"]), "portool");
    }
}
