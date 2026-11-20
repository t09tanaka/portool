//! The `portool` command line. All logic lives in the library crate; this
//! binary only maps the CLI's exit code onto the process.

fn main() {
    let code = portool::__run_cli();
    if code != 0 {
        std::process::exit(code);
    }
}
