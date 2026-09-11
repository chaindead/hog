//! Thin entry point: `argv` -> [`hog::run`] -> [`ExitCode`].
//!
//! Everything that could conceivably be tested lives in the library crate
//! (`proj-lib-main-split`). This file must stay boring.

use std::process::ExitCode;

fn main() -> ExitCode {
    // Not `Cli::parse()`: `--help` prints the `command` template from the user's
    // own config (HLD §6), so argv is parsed twice — once loosely for
    // `--config`, once for real with the help text already built.
    //
    // A usage error never reaches us: clap prints it and exits with 2 on its
    // own, which is exactly the code the exit-code table (HLD §6) demands.
    let cli = hog::help::parse();

    match hog::run(cli) {
        Ok(code) => code,
        // The single place that turns a domain error into stderr text and an
        // exit code. `report` must not use `println!` — see `output`.
        Err(err) => hog::report(&err),
    }
}
