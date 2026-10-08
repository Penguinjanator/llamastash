//! `llamastash completions` — print a shell completion script to stdout.
//!
//! Static output built from the clap spec, so it needs no daemon, no
//! config and no network, and writes nothing. Model references are
//! dynamic, so they stay uncompleted.

use std::io::Write;

use clap::CommandFactory;
use clap_complete::Generator;

use crate::cli::cli_args::{Cli, CompletionsArgs};
use crate::cli::exit_codes::{self, CliExit, CliResult};

pub fn handle(args: CompletionsArgs) -> CliResult {
  let mut cmd = Cli::command();
  let name = cmd.get_name().to_string();
  cmd.set_bin_name(&name);
  cmd.build();
  // Render into memory first: `Shell::generate` panics on a write error,
  // so streaming straight to stdout would abort on
  // `llamastash completions bash | head -1` (EPIPE), a closed terminal, or
  // a full disk. `try_generate` reports it instead.
  let mut script = Vec::new();
  args
    .shell
    .try_generate(&cmd, &mut script)
    .map_err(write_err)?;
  let stdout = std::io::stdout();
  let mut out = stdout.lock();
  out.write_all(&script).map_err(write_err)?;
  out.flush().map_err(write_err)
}

fn write_err(e: impl std::error::Error) -> CliExit {
  CliExit::new(
    exit_codes::UNKNOWN,
    format!("could not write the completion script: {e}"),
  )
}
