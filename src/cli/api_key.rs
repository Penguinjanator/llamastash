//! `llamastash api-key` — print the proxy's bearer key.
//!
//! Exists so a client config can resolve the key at runtime instead of
//! carrying a copy: pi.dev's `apiKey: "!llamastash api-key"` shells out to
//! this, which beats both writing the secret into a config file and making
//! the user export an env var from their shell rc before the integration
//! works.
//!
//! The bare key reads the resolved config only, never the daemon, so it
//! stays inside a client's short shell-out timeout. `--json` also asks a
//! running daemon for its proxy address.

use serde_json::json;

use crate::cli::cli_args::{ApiKeyArgs, Cli};
use crate::cli::exit_codes::CliResult;
use crate::cli::output::pretty_json;
use crate::config::Config;
use crate::init::external::proxy_url;

/// What a keyless loopback proxy hands out. It ignores the value, but
/// clients that refuse to start without a non-empty key need something —
/// the same stub the `env.sh` writer emits.
pub const KEYLESS_STUB: &str = "llamastash";

pub async fn handle(args: ApiKeyArgs, _cli: &Cli, config: &Config) -> CliResult {
  let configured = config.proxy.effective_api_key();
  let key = configured
    .clone()
    .unwrap_or_else(|| KEYLESS_STUB.to_string());
  if args.json {
    // The proxy can listen past a busy configured port, or on one a
    // `daemon start --proxy-port` set without touching config.
    let base_url = match proxy_url::from_running_daemon().await {
      Some(url) => url,
      None => proxy_url::from_config(config),
    };
    let out = json!({
      "api_key": key,
      "auth": if configured.is_some() { "enforced" } else { "off" },
      "base_url": base_url,
    });
    println!("{}", pretty_json(&out));
  } else {
    // Bare value on one line: this is consumed by `$(...)` and by client
    // configs that shell out for their credential.
    println!("{key}");
  }
  Ok(())
}
