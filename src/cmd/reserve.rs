//! `portool reserve` / `unreserve` (external review P1-9): persistent port
//! reservations for services portool must never allocate over (a stopped
//! Postgres on 5432 looks "free" to a bind check).

use crate::config::Config;
use crate::error::{Error, Result};
use crate::lock;
use crate::paths;
use crate::registry::{overlaps, Registry, Reservation};
use crate::store;
use std::time::Duration;

const LOCK_TIMEOUT: Duration = Duration::from_secs(10);

pub fn reserve(spec: &str, label: Option<String>) -> Result<()> {
    let block = parse_block(spec)?;
    let config = Config::load()?;
    let _lock = lock::acquire(&paths::lock_path()?, LOCK_TIMEOUT)?;
    let registry_path = paths::registry_path()?;
    let mut registry =
        store::load_strict(&registry_path)?.unwrap_or_else(|| Registry::empty(config.range));

    if let Some(existing) = registry.reservations.iter_mut().find(|r| r.block == block) {
        if label.is_some() && label != existing.label {
            existing.label = label;
            registry.validate()?;
            store::save(&registry_path, &registry)?;
            println!(
                "portool: {}-{} was already reserved; label updated",
                block.0, block.1
            );
        } else {
            println!("portool: {}-{} is already reserved", block.0, block.1);
        }
        return Ok(());
    }
    if let Some(other) = registry.all_blocks().iter().find(|&&b| overlaps(b, block)) {
        return Err(Error::General(format!(
            "cannot reserve {}-{}: overlaps existing allocation or reservation {}-{}",
            block.0, block.1, other.0, other.1
        )));
    }

    registry.reservations.push(Reservation {
        block,
        label,
        pinned: true,
    });
    registry.validate()?;
    store::save(&registry_path, &registry)?;
    println!("portool: reserved {}-{}", block.0, block.1);
    Ok(())
}

pub fn unreserve(spec: &str) -> Result<()> {
    let block = parse_block(spec)?;
    // Derived from the spec's syntax, not the parsed values: a degenerate
    // RANGE spec like "19003-19003" must still require an exact match, so it
    // can never containment-match and remove a wider reservation (e.g.
    // "19000-19009"). Only a bare PORT spec (no '-') containment-matches.
    let single_port = !spec.contains('-');
    let _lock = lock::acquire(&paths::lock_path()?, LOCK_TIMEOUT)?;
    let registry_path = paths::registry_path()?;
    let mut registry = store::load_strict(&registry_path)?
        .ok_or_else(|| Error::General("no registry exists; nothing is reserved".to_string()))?;

    let index = registry.reservations.iter().position(|r| {
        if single_port {
            r.block.0 <= block.0 && block.0 <= r.block.1
        } else {
            r.block == block
        }
    });
    match index {
        Some(index) => {
            let removed = registry.reservations.remove(index);
            store::save(&registry_path, &registry)?;
            println!(
                "portool: unreserved {}-{}",
                removed.block.0, removed.block.1
            );
            Ok(())
        }
        None => Err(Error::General(format!(
            "no reservation matches {spec} (see 'portool ls --all --json')"
        ))),
    }
}

/// Parses `"5432"` (single port) or `"6000-6009"` (inclusive range).
fn parse_block(spec: &str) -> Result<(u16, u16)> {
    let invalid = || {
        Error::General(format!(
            "invalid port spec '{spec}' (expected PORT or START-END)"
        ))
    };
    let block = match spec.split_once('-') {
        Some((start, end)) => (
            start.trim().parse::<u16>().map_err(|_| invalid())?,
            end.trim().parse::<u16>().map_err(|_| invalid())?,
        ),
        None => {
            let port = spec.trim().parse::<u16>().map_err(|_| invalid())?;
            (port, port)
        }
    };
    if block.0 == 0 {
        return Err(Error::General("port 0 cannot be reserved".to_string()));
    }
    if block.0 > block.1 {
        return Err(Error::General(format!(
            "invalid port range '{spec}' (start exceeds end)"
        )));
    }
    Ok(block)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_block_accepts_single_port_and_range() {
        assert_eq!(parse_block("5432").unwrap(), (5432, 5432));
        assert_eq!(parse_block("6000-6009").unwrap(), (6000, 6009));
        assert!(parse_block("0").is_err());
        assert!(parse_block("6009-6000").is_err());
        assert!(parse_block("abc").is_err());
        assert!(parse_block("1-2-3").is_err());
    }
}
