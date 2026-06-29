//! Thin binary entrypoint. All logic lives in the `actstat` library so it can
//! be unit-tested directly; `main` just parses args, runs, and forwards the
//! resulting process exit code.

use std::process::ExitCode;

fn main() -> ExitCode {
    actstat::cli::run()
}
