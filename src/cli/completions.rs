//! `llamastash completions` — print a shell completion script to stdout.
//!
//! Static output built from the clap spec, so it needs no daemon, no
//! config and no network, and writes nothing. Model references are
//! dynamic, so they stay uncompleted.

use clap::CommandFactory;

use crate::cli::cli_args::{Cli, CompletionsArgs};
use crate::cli::exit_codes::CliResult;

pub fn handle(args: CompletionsArgs) -> CliResult {
  let mut cmd = Cli::command();
  let name = cmd.get_name().to_string();
  clap_complete::generate(args.shell, &mut cmd, name, &mut std::io::stdout());
  Ok(())
}
