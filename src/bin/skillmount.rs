//! Fallback `SkillMount` executable shim.

use std::process::ExitCode;

fn main() -> ExitCode {
    skillmount::run_from(std::env::args_os())
}
