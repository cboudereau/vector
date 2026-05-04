use clap::Parser;
use vrl::cli::{Opts, cmd::cmd};

fn main() {
    let functions = sol_vrl_functions::all();
    std::process::exit(cmd(&Opts::parse(), functions));
}
