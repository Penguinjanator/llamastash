use anyhow::Result;
use llamastash::{cli, config::loader, util::logging};

fn main() -> Result<()> {
  limit_malloc_arenas();
  tokio::runtime::Builder::new_multi_thread()
    .enable_all()
    .build()?
    .block_on(run())
}

/// glibc's default of 8 arenas per core keeps memory the startup scan's
/// parallel header parses freed. Set in-process rather than through
/// `MALLOC_ARENA_MAX` so spawned model servers keep glibc's default, and before
/// the runtime starts so no worker thread has taken an arena yet. A limit the
/// user set is left alone: glibc has already applied it, and `mallopt` would
/// overwrite it.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn limit_malloc_arenas() {
  let user_set = std::env::var_os("MALLOC_ARENA_MAX").is_some()
    || std::env::var("GLIBC_TUNABLES").is_ok_and(|t| t.contains("glibc.malloc.arena_max"));
  if user_set {
    return;
  }
  // SAFETY: `mallopt` only sets an allocator parameter; no pointers involved.
  unsafe {
    libc::mallopt(libc::M_ARENA_MAX, 2);
  }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn limit_malloc_arenas() {}

async fn run() -> Result<()> {
  // Translate `LLAMASTASH_OFFLINE=1`/`0`/empty into the `true`/unset clap's
  // boolean env binding accepts, before parsing argv (see the fn doc).
  cli::cli_args::normalize_offline_env();

  // Parse by hand so clap's arg-rejection exit code matches our contract:
  // a usage error exits USAGE (64), not clap's default 2. `--help` /
  // `--version` are not errors — clap writes them to stdout and we exit 0.
  // `parse_cli` also wires the `--no-colors` → `ColorChoice::Never` policy
  // for styled help, which has to be decided before clap renders --help.
  let cli = match cli::cli_args::parse_cli() {
    Ok(cli) => cli,
    Err(err) => {
      if !err.use_stderr() {
        let _ = err.print();
        std::process::exit(0);
      }
      if cli::cli_args::argv_wants_json() {
        let text = err.render().to_string();
        // The error is the first paragraph; the `Usage:` block follows a blank line.
        let head = text
          .lines()
          .take_while(|l| !l.trim().is_empty())
          .map(str::trim)
          .collect::<Vec<_>>()
          .join(" ");
        let message = head.strip_prefix("error: ").unwrap_or(&head);
        cli::output::print_json_error(cli::exit_codes::USAGE, message);
      } else {
        let _ = err.print();
      }
      std::process::exit(cli::exit_codes::USAGE);
    }
  };

  // Logger must be initialised BEFORE the panic hook — `log::error!` inside
  // the hook is a silent no-op while no logger is registered, so a panic
  // during CLI parsing/early startup would otherwise leave no trace in the
  // log file. Both calls are best-effort: a missing log dir or an already
  // initialised logger shouldn't block CLI use.
  let _ = logging::init(cli.verbose);
  logging::install_panic_hook();

  let config = loader::load_config(cli.config.clone());
  let code = cli::dispatch(cli, config).await?;
  std::process::exit(code);
}
