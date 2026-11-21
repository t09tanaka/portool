//! `portool::run_cli` runs the command line in-process with the given
//! arguments, as a multi-call binary invoked through a `portool` link does.

#[test]
fn run_cli_maps_exit_codes_like_the_binary() {
    // The program name is ignored, so a link from another binary works.
    assert_eq!(portool::run_cli(["belt", "--version"]), 0);
    assert_eq!(portool::run_cli(["portool", "--help"]), 0);
    // Usage errors exit 64 (EX_USAGE), not clap's default 2.
    assert_eq!(portool::run_cli(["portool", "--no-such-flag"]), 64);
    assert_eq!(portool::run_cli(["portool", "no-such-command"]), 64);
}
