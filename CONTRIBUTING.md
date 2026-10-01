# Contributing to llamastash

Thanks for the interest. llamastash is still pre-1.0 and the API surface is moving, so coordination matters more than usual right now.

## Before you start

- Read [`docs/brainstorms/llamatui-requirements.md`](docs/brainstorms/llamatui-requirements.md) for the scope of v1 and what's deferred.
- Skim the implementation plan in [`docs/plans/2026-05-13-001-feat-llamatui-v1-launcher-plan.md`](docs/plans/2026-05-13-001-feat-llamatui-v1-launcher-plan.md). If your contribution lands inside an existing Implementation Unit, mention which one in the PR description.
- Open an issue (or comment on an existing one) before opening a PR for anything bigger than a typo or a one-file bug fix. Saves us both time when scope or approach needs to be discussed.

## Repo layout

- `src/` — production code. Module boundaries mirror the plan's Implementation Units: `daemon/`, `discovery/`, `gguf/`, `gpu/`, `ipc/`, `launch/`, `tui/`, `cli/`, `theme/`, `config/`, `util/`.
- `tests/` — integration tests. The fake `llama-server` lives in `tests/fixtures/fake_llama_server.rs` and ships only when built with the `test-fixtures` feature.
- `docs/` — user-facing docs (`usage.md`, `architecture.md`, `troubleshooting.md`) and design history (`brainstorms/`, `plans/`).
- `.github/workflows/` — CI and release workflows.

## Building

```bash
cargo build
make test                                  # lint + nextest + doctests, as CI runs them (cargo install cargo-nextest)
cargo test --features test-fixtures        # includes the integration suite
cargo clippy --all-targets -- -D warnings  # CI pins this
cargo fmt --check                          # CI pins this
```

The integration tests need `--features test-fixtures` because they spawn the `fake_llama_server` binary the daemon launches in place of a real llama.cpp child.

### Running the daemon locally

Two terminals are typically the easiest setup when you're hacking on the daemon or IPC surface:

```bash
# Terminal 1: foreground daemon. Logs land in your terminal; Ctrl-C stops it.
cargo run -- daemon start

# Terminal 2: drive it with the CLI or TUI.
cargo run -- list
cargo run -- start <model>
cargo run                              # opens the TUI against the same daemon
cargo run -- daemon status             # pid / uptime / connections
cargo run -- daemon stop               # graceful shutdown
```

The daemon binds two loopback HTTP listeners: the control plane (default `127.0.0.1:48134`) and the OpenAI-compat proxy (default `127.0.0.1:11434/11435`). Connection details — URL + bearer token — live in `runtime.json` under the state dir (`$XDG_STATE_HOME/llamastash` on Linux, `~/Library/Application Support/llamastash` on macOS, `%APPDATA%\llamastash\data` on Windows). Clients read it to attach.

If you need two daemons side-by-side (e.g. testing migrations), give each its own state dir:

```bash
LLAMASTASH_STATE_DIR=/tmp/llamastash-dev cargo run -- daemon start
LLAMASTASH_STATE_DIR=/tmp/llamastash-dev cargo run -- list
```

The daemon allocates its own control-plane port if `48134` is taken (random in `41100..=41300`); concurrent state dirs never collide. If something is wedged and the normal `daemon stop` won't go through, `cargo run -- daemon stop --force` falls back to a PID-targeted signal; alternatively, delete `runtime.json` and `daemon.pid` in the state dir — the next `daemon start` re-binds clean.

## Code conventions

- Rust edition 2021. Minimum supported Rust version is pinned in `Cargo.toml` (`rust-version`).
- Two-space indentation, `rustfmt.toml` is authoritative. `cargo fmt` before pushing.
- `cargo clippy -- -D warnings` is enforced in CI. No `#[allow(...)]` without a one-line reason.
- Inline tests under `#[cfg(test)] mod tests` per file; integration tests in `tests/`. Either is fine; pick whichever keeps the test close to the behavior it exercises.
- Comments explain **why**, not **what**. Identifiers should already say what.

## Submitting a PR

1. Branch off `main`.
2. Keep commits logically split — one cohesive change per commit when reasonable. Conventional commit prefixes (`feat:`, `fix:`, `refactor:`, `test:`, `docs:`) are encouraged.
3. Make sure `make test` passes locally.
4. Open the PR against `main`. The description should at minimum say *what* the change does, *why*, and how it was tested.

## Security

If you find a security issue, please follow the disclosure process in [`SECURITY.md`](SECURITY.md) instead of opening a public issue.

## License

By contributing, you agree your contribution is licensed under the MIT license, the same license the rest of the project uses.
