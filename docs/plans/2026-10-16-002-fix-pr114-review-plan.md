# PR #114 review fixes: address matching, the withheld-repair ledger, and one shared walk

Goal: land the reviewable items from the PR #114 review that are worth the change,
without touching what the review found correct.

Base: `edd06fb` (PR #114 head). This stack sits on the open PR, so the branch carries
its six commits until #114 merges.

Five items. Three lanes, one writer per file.

- `stale_url.rs`'s URL regex swallows bytes past the URL, so a comma-separated line
  under-repairs and the finding prints bytes that are not part of the URL.
- Within the six-port window on the live host, a port is assumed to be the proxy's
  earlier self, so a second in-window listener (a second daemon, per
  `docs/usage.md`) is repointed. The docs promise the opposite scope. Say the
  residual instead of pretending it is not there.
- `apply_fixes` says nothing when a `tool_config_proxy_url` finding shows up with no
  live proxy, against its own contract ("`--fix` is never silent about what it left
  alone").
- `stale_url::config_files` and the wizard walk the same patchers and companions with
  the same rules, twice. The guarantee ("exactly the path `init` would write") now has
  two copies.
- `FEATURES.md`'s doctor finding-id list is behind, and the doc comment on the new
  finding id names a flag that does not exist (`--host`; the real one is
  `--proxy-host`).

## Decisions

- **D1 — narrow the tail, not the match.** The tail exists to swallow the path, quote
  or bracket that follows the port. Excluding `,` `]` `)` `}` from it is enough: those
  four close a YAML flow list, a JSON array or an inline table. The URL still ends at
  a quote, whitespace or `<`.
- **D2 — do not try to recognise a second proxy.** Teaching the check to tell "the
  proxy's old port" from "another daemon on an in-window port" needs to read the other
  daemon's state file, which is another user's, and a wrong guess repoints a working
  config. The rewrite is already ledged and names both addresses. Document the
  residual and move on.
- **D3 — the shared walk returns owned patchers** (`Vec<Box<dyn ToolPatcher>>`), the
  same shape as `tools::registered()`, so both callers swap in one line and keep their
  own loops. It lives in its own module because the wizard and `stale_url` are both
  callers.
- **D4 — no new fix id and no schema change.** These are corrections to behavior that
  shipped in #114, not new surface.

## Units

### U-A (lane `fix-a`) — address matching and the shared walk's stale side

Owns `src/init/external/stale_url.rs`, `src/init/external/proxy_url.rs`.

1. Tighten the tail class in the URL regex to `([^\s"'<>,\\]\\)\\}]*)` and add tests:
   a YAML flow list holding two URLs on one line yields both and repairs both; a URL
   inside a JSON array or an inline table yields the URL and nothing more; a bracketed
   IPv6 config URL matches a bracketed IPv6 live address (read
   `LiveProxy::from_base_url` first and assert what it actually does, do not guess).
2. `config_files()` delegates its patcher/companion walk to
   `super::tool_targets::tool_targets()`, keeping its `is_file()` filter and its dedupe
   so a shared file (`env.sh`, written by both `env-sh` and pi.dev's companion) is
   scanned once. Behavior must not change. Add a test that the scan never yields a
   missing path and never yields the same path twice.
3. Rename `--host` to `--proxy-host` in this module's header comment.

### U-B (lane `fix-b`) — the withheld repair, and docs that match the code

Owns `src/init/doctor.rs`, `docs/usage.md`, `docs/troubleshooting.md`, `CHANGELOG.md`.

1. In `apply_fixes`, `FixId::ToolConfigProxyUrl` with no live proxy pushes a
   `FixReport::skipped` naming why, instead of staying silent. Test it.
2. Findings and docs name both addresses in the plain form too, not only inside the
   `old → new` arrow, so a wrong rewrite is obvious in the report.
3. State the residual in `docs/usage.md` and `docs/troubleshooting.md`: inside the
   window on the live host a port is taken to be the proxy's earlier self, so a second
   listener on such a port gets repointed; a rewrite names both addresses.
4. `docs/usage.md` names `--proxy-port` / `--host` where the real flag is
   `--proxy-host`. Fix it.
5. `CHANGELOG.md` one-liners for the user-visible fixes in this stack.

### U-C (lane `fix-c`) — the wizard's walk, and the stale FEATURES.md list

Owns `src/init/wizard.rs`, `src/init/external/tool_targets.rs`, `FEATURES.md`, `TODO.md`.

1. Replace the wizard's inline `once(patcher).chain(companions)` walk with
   `tool_targets::tool_targets()`, same order, same summary rows.
2. Unit test for `tool_targets()`: a companion sits beside the patcher that pulls it
   in, the list is at least as long as `tools::registered()`, and order is stable.
3. `FEATURES.md` § `llamastash doctor`: list the finding ids the code declares
   (`rg -n '=> "' src/init/doctor.rs` is the source of truth, plus the ids backends
   contribute) and say plainly which ones `doctor --fix` repairs. Anything that cannot
   be pinned gets a one-line `TODO.md` entry, not a guess.

## Verification

From the worktree root. Isolate anything that touches a daemon
(`LLAMASTASH_STATE_DIR`, `LLAMASTASH_CONFIG_DIR`, scratch `HOME`, non-default
`--proxy-port`); never the installed binary, never the user's real daemon.

- `cargo nextest run --features test-fixtures --lib stale_url`
- `cargo nextest run --features test-fixtures --lib doctor`
- `cargo nextest run --features test-fixtures --lib tool_targets`
- `cargo nextest run --features test-fixtures --lib wizard`
- `cargo nextest run --features test-fixtures --lib external`
- `make lint`
- Full suite once at the end (pre-push covers it; `--no-verify` plus CI if the local
  daemon-spawn tests flake as they did on #114).
- Spot check for U-B: with a scratch daemon on a non-default port, `doctor --json` on a
  config naming an in-window port and a comma-separated line, showing both URLs found
  and both repaired.

## Definition of done

All five items landed or explicitly rejected with a reason, docs and code in the same
commit per the repo rule, the commands above green, commits on `fix/pr114-review`,
nothing pushed.
