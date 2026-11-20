//! Advisory warnings and hints.
//!
//! Code throughout the crate reports a non-fatal warning with [`emit`]. By
//! default (the CLI) the line goes straight to standard error, exactly as
//! before. A library entry point wraps its work in [`collect`], which
//! diverts the lines on the current thread into a `Vec` for the caller
//! instead, so an embedding program never sees stray stderr output.

use std::cell::RefCell;
use std::fmt;

thread_local! {
    static SINK: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
}

/// Reports one warning line: collected if a [`collect`] scope is active on
/// this thread, otherwise printed to standard error.
pub(crate) fn emit(args: fmt::Arguments<'_>) {
    let line = args.to_string();
    let printed = SINK.with(|sink| match sink.borrow_mut().as_mut() {
        Some(lines) => {
            lines.push(line.clone());
            true
        }
        None => false,
    });
    if !printed {
        eprintln!("{line}");
    }
}

/// Restores the previous sink when a [`collect`] scope ends, even on panic.
struct Restore(Option<Vec<String>>);

impl Drop for Restore {
    fn drop(&mut self) {
        let previous = self.0.take();
        SINK.with(|sink| *sink.borrow_mut() = previous);
    }
}

/// Runs `f`, returning its result together with every warning emitted on
/// this thread while it ran. Nesting is allowed.
pub(crate) fn collect<T>(f: impl FnOnce() -> T) -> (T, Vec<String>) {
    let previous = SINK.with(|sink| sink.borrow_mut().replace(Vec::new()));
    let guard = Restore(previous);
    let value = f();
    let lines = SINK
        .with(|sink| sink.borrow_mut().take())
        .unwrap_or_default();
    drop(guard);
    (value, lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_captures_and_restores() {
        let (value, lines) = collect(|| {
            emit(format_args!("a {}", 1));
            let (_, inner) = collect(|| emit(format_args!("inner")));
            assert_eq!(inner, vec!["inner"]);
            emit(format_args!("b"));
            7
        });
        assert_eq!(value, 7);
        assert_eq!(lines, vec!["a 1", "b"]);
        SINK.with(|s| assert!(s.borrow().is_none()));
    }
}
