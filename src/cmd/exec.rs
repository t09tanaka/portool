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
/// `strict` turns the execution-boundary bind conflict (batch C #1) into a
/// hard failure; `reallocate_on_conflict` moves the worktree to a fresh
/// block instead of warning. Both default off.
pub fn run(
    env_files: &[PathBuf],
    command: &[OsString],
    strict: bool,
    reallocate_on_conflict: bool,
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

    // Batch C #1: exec is the execution boundary -- the one place worth
    // verifying the block's ports are actually free right now. A conflict
    // here may just be this worktree's own already-running server, so the
    // default is a neutral advisory, not a failure or a silent move.
    let outcome = if ports::block_free(outcome.block) {
        outcome
    } else if reallocate_on_conflict {
        sync::reallocate(&ctx, true)?
    } else {
        eprintln!(
            "portool: ports {}-{} are in use -- this may be this worktree's own running \
             processes (pass --reallocate-on-conflict to move, --strict to fail)",
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

    let (map, to_set) = compose(file_entries, parent, portool_vars);
    let mut resolved = envread::resolve(&map)?;
    resolved.retain(|name, _| to_set.contains(name));

    exec_command(command, &resolved)
}

/// Merges the three variable sources into one map with the spec §6
/// precedence: earlier env files < later env files (later lines win
/// within a file too) < parent environment < portool-managed variables.
///
/// Also returns the set of names to actually set on the child: file and
/// portool winners only. Parent-sourced winners are excluded — the child
/// inherits them as-is, which keeps non-UTF-8 parent values byte-exact.
fn compose(
    file_entries: Vec<Vec<(String, FileValue)>>,
    parent: Vec<(String, String)>,
    portool_vars: Vec<(String, String)>,
) -> (BTreeMap<String, Entry>, BTreeSet<String>) {
    let mut map = BTreeMap::new();
    let mut to_set = BTreeSet::new();
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
        );
        let resolved = envread::resolve(&map).unwrap();
        assert_eq!(resolved["URL"], "localhost:3005");
        assert_eq!(resolved["TEST_DB_PORT"], "3005");
    }
}
