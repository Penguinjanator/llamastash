//! The proxy URL written into tool configs.
//!
//! The running daemon's address comes first: the proxy moves up to five
//! ports past a busy one, and `daemon start --proxy-port` / `--host` never
//! reach `config.yaml`, so config alone can name a port nothing serves.
//! Config, then the built-in default, cover a daemon that cannot be
//! reached.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use serde_json::Value;

use crate::cli::cli_args::Cli;
use crate::config::Config;

/// The URL to write, plus a note when it is not one the proxy serves
/// right now.
pub struct ProxyUrl {
  pub base_url: String,
  pub note: Option<String>,
}

pub async fn resolve(cli: &Cli, config: &Config) -> ProxyUrl {
  let fallback = from_config(config);
  let status = match crate::cli::client::connect_or_spawn(cli, config).await {
    Ok(mut client) => client.call("status", None).await.map_err(|e| e.to_string()),
    Err(e) => Err(e.message.unwrap_or_else(|| format!("exit {}", e.code))),
  };
  match status {
    Ok(body) => match from_status(&body) {
      Some(base_url) => ProxyUrl {
        base_url,
        note: None,
      },
      None => {
        let state = body
          .pointer("/proxy/status")
          .and_then(Value::as_str)
          .unwrap_or("unknown");
        ProxyUrl {
          note: Some(format!(
            "the daemon's proxy is not listening ({state}); tool configs point at {fallback}"
          )),
          base_url: fallback,
        }
      }
    },
    Err(e) => ProxyUrl {
      note: Some(format!(
        "could not read the proxy address from the daemon ({e}); tool configs point at {fallback}"
      )),
      base_url: fallback,
    },
  }
}

/// The address a running daemon's proxy listens on. Never spawns a
/// daemon, and waits at most 2 s, for `api-key --json`.
pub async fn from_running_daemon() -> Option<String> {
  let dir = crate::util::paths::state_dir()?;
  crate::daemon::existing_daemon_pid(&dir)?;
  let mut client = crate::ipc::Client::connect(&dir).await.ok()?;
  let status = client
    .call_with_timeout("status", None, std::time::Duration::from_secs(2))
    .await
    .ok()?;
  from_status(&status)
}

/// The address a `listening` proxy reports in `status`. Any other state
/// carries the address it tried to bind, which serves nothing.
fn from_status(status: &Value) -> Option<String> {
  let proxy = status.get("proxy")?;
  if proxy.get("status").and_then(Value::as_str) != Some("listening") {
    return None;
  }
  let addr: SocketAddr = proxy.get("listen")?.as_str()?.parse().ok()?;
  Some(base_url(addr))
}

pub fn from_config(config: &Config) -> String {
  base_url(SocketAddr::new(
    config.proxy.effective_host(),
    config.proxy.effective_port(),
  ))
}

/// A wildcard bind is reached over loopback; a specific address is used
/// as is, since a proxy bound to one LAN address does not answer on
/// loopback.
fn base_url(mut addr: SocketAddr) -> String {
  match addr.ip() {
    IpAddr::V4(ip) if ip.is_unspecified() => addr.set_ip(Ipv4Addr::LOCALHOST.into()),
    IpAddr::V6(ip) if ip.is_unspecified() => addr.set_ip(Ipv6Addr::LOCALHOST.into()),
    _ => {}
  }
  format!("http://{addr}/v1")
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  fn status(state: &str, listen: &str) -> Value {
    json!({ "proxy": { "status": state, "listen": listen } })
  }

  #[test]
  fn a_listening_proxy_gives_its_bound_port() {
    // The proxy moved past a busy 11435.
    assert_eq!(
      from_status(&status("listening", "127.0.0.1:11437")).as_deref(),
      Some("http://127.0.0.1:11437/v1")
    );
  }

  #[test]
  fn a_proxy_that_did_not_bind_gives_nothing() {
    for state in ["port_in_use", "unbound", "refused_insecure"] {
      assert!(from_status(&status(state, "127.0.0.1:11435")).is_none());
    }
    assert!(from_status(&json!({ "proxy": { "status": "disabled", "listen": null } })).is_none());
    assert!(from_status(&json!({})).is_none(), "no proxy block");
  }

  #[test]
  fn wildcard_binds_map_to_loopback_and_specific_hosts_stay() {
    assert_eq!(
      from_status(&status("listening", "0.0.0.0:11435")).as_deref(),
      Some("http://127.0.0.1:11435/v1")
    );
    assert_eq!(
      from_status(&status("listening", "[::]:11435")).as_deref(),
      Some("http://[::1]:11435/v1")
    );
    assert_eq!(
      from_status(&status("listening", "192.168.1.20:11435")).as_deref(),
      Some("http://192.168.1.20:11435/v1")
    );
  }

  #[test]
  fn config_falls_back_to_the_default_port() {
    let mut config = Config::default();
    assert_eq!(from_config(&config), "http://127.0.0.1:11435/v1");
    config.proxy.ollama_compat = true;
    assert_eq!(from_config(&config), "http://127.0.0.1:11434/v1");
    config.proxy.port = Some(12000);
    assert_eq!(from_config(&config), "http://127.0.0.1:12000/v1");
  }
}
