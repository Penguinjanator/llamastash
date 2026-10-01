# E2E plan: generic backend (`feat/generic-backend`)

Claim under test: **a server declared in `config.yaml` runs like any built-in
backend on every surface** (CLI, TUI, `--json`, presets, proxy), including the
review fixes in `a2c14b1b` (`{name}` = published id, TUI restart waits the stop
grace) and any fixes from the ultrareview.

## Setup

- Binary: `target/debug/llamastash` built from the branch head. Never bare `llamastash`.
- Isolation: `LLAMASTASH_STATE_DIR` / `CONFIG_DIR` / `CACHE_DIR` / `HF_HOME` under
  one root in `~/.cache/ls-uat-generic/`, re-exported in every shell call. Proxy
  `:21435`, control `:21436`. The user's daemon (`:11435`) is never touched; hash
  `~/.config/llamastash/config.yaml` before and after.
- Scan roots: two isolated dirs, `root-a/` and `root-b/`, each holding a symlink to
  the same `Llama-3.2-1B-Instruct-Q4_K_M.gguf` (stem collision), plus one symlink
  to the Flash-Next UD-Q4_K_XL split set in `root-a/` for the gufo case.
- Engines (real):
  - `llama-server` build 11200 (`81bc6b83f`, 0.5.0-dev; upstream latest tag v0.5.0)
  - gufo `d9a84f1` (2026-09-26), `release-gcc15` build
  - Halogen `0.14.0` (latest image tag), Docker wrapper `~/.local/bin/halogen-serve`
  - CIRU `3cf984c` (upstream HEAD), wrapper from the `docs/usage.md` example
- Generic entries in the isolated `config.yaml`:
  - `ls-bound`: `llama-server`, `model: "Llama-3.2-1B*"`, args `--port {port} -m {model} --alias {name}`,
    knobs `{flag: --ctx-size, id: ls-ctx, ctx: true}`, `{flag: --seed, id: ls-seed}`,
    `{flag: --temp, id: ls-temp, default: "0.7"}` (ids can't reuse built-in knob ids)
  - `ls-free`: `llama-server`, no `model`, fixed `-m <1B path> --alias {name}`, env `LS_UAT_CTX: "{ls-ctx}"`
  - `gufo`: copy of the user's real entry (model-bound to Flash-Next, `stop_grace_secs: 60`)
- Every argv/env claim is read from `/proc/<pid>/cmdline` and `/proc/<pid>/environ`,
  never from llamastash's own output.

## Cases

| # | Surface | What it proves |
|---|---|---|
| 1 | daemon | Starts clean on the isolated config; `doctor` lists the entries without errors |
| 2 | CLI | `list` shows `ls-free` as its own row; the 1B rows offer `generic-ls-bound` as a server; `list --json` shape |
| 3 | CLI | `knobs` / `knobs --json` list the declared knobs per entry; `start --help` has no flags for them (by design) |
| 4 | CLI | `start <1B root-a> --server generic-ls-bound --ctx 4096`: real argv has `--ctx-size 4096 --temp 0.7`, no `--seed` (no value, no default), `-m` = root-a path |
| 5 | `{name}` | Case 4's `--alias` is the repo-qualified id, equal to the id `/v1/models` lists for that row (stems collide) |
| 6 | proxy | Chat completion through `:21435` with that id returns 200 from the real engine; the bare stem answers `ambiguous_model` |
| 7 | named | `start … --name uat`: `--alias` = `<published>@uat`, listed in `/v1/models`, routable |
| 8 | env | `start ls-free`: `LS_UAT_CTX` in `/proc/<pid>/environ` = the ctx knob; `{name}` = `ls-free` |
| 9 | extras | `start <1B> --server generic-ls-bound -- --seed 5`: `--seed 5` reaches argv as a raw flag; knob `ls-seed` not set, not remembered |
| 10 | JSON | `status --json`: `backend: generic`, `params.knobs`, `stop_grace_secs` on the row; `stop --json` shape |
| 11 | presets | A preset with `server: generic-ls-bound` + `ls-seed: 7` (via `presets save`, else hand-written): `presets show --json` reads it back; `--preset` gives case 4's argv plus `--seed 7` |
| 12 | presets | Comments in the isolated `config.yaml` survive `presets save`; the `backend.generic` block is untouched |
| 13 | last-used | Restart the daemon; a bare `start` of the same row reuses server + knobs |
| 14 | TUI | Server picker shows the generic server on the 1B row; Settings editor shows the declared knobs; a knob edited in the TUI reaches argv |
| 15 | TUI | Running panel shows `generic`, ctx, and the knob values for a live launch |
| 16 | gufo | Flash-Next via `generic-gufo`: `--served-model-name` = the published id; a proxied request returns 200 (gufo checks `model`) |
| 17 | stop grace | With gufo loaded, TUI daemon restart (keymap action) completes and the new daemon comes up, even when gufo takes >8 s to stop |
| 18 | CLI stop | `daemon stop` waits for gufo's exit and prints `daemon: stopped` |
| 19 | safety | `--host 0.0.0.0` in extras refused for a generic launch |
| 20 | isolation | The user's `config.yaml` hash unchanged; their daemon still up on `:11435` |
| 21 | Halogen | Own row via Docker: `{name}` and knobs reach the container env; loopback-only port; proxied chat 200; TUI restart with it loaded |
| 22 | CIRU | Own row via an `exec` wrapper: knobs reach env and CIRU's argv; proxied chat 200; `stop` ends the engine |

## Rule

Iterate until every case is green. A failure that the test suite missed gets a
regression test before the fix.

## Results: 2026-09-27, all green

Engines: llama-server build 11200 (`81bc6b83f`), gufo `d9a84f1`. Models:
Llama-3.2-1B-Instruct-Q4_K_M (same file in two roots), Qwen3.8-Flash-Next-UD-Q4_K_XL
(104 GB, gufo ctx 8192, no MTP head). Argv and env read from `/proc`.

| # | Result |
|---|---|
| 1 | `doctor` found all 3 binaries; 3 servers configured |
| 2 | `ls-free` is its own row; both 1B rows and Flash-Next offer `llamacpp|generic` |
| 3 | 6 knobs listed across 3 entries, human + `--json` |
| 4 | `--ctx-size 4096 --temp 0.7`, no `--seed`, `-m` = root-a path |
| 5 | `--alias root-a/Llama-3.2-1B-Instruct-Q4_K_M`, the id `/v1/models` lists |
| 6 | Published id: 200 with a real completion. Bare stem: `ambiguous_model` listing both ids |
| 7 | `--alias …@uat`, listed and routable (200) |
| 8 | `LS_UAT_CTX=2048` in the engine env; `--alias ls-free`; the knob is not also emitted as a flag |
| 9 | `-- --seed 5` reached argv as-is; `ls-seed` stayed unset. It is reused on a bare `start`, like any extra (doc wording fixed) |
| 10 | `status --json`: `backend: generic`, `params.knobs`, `extras`, `stop_grace_secs: 60` on the gufo row |
| 11 | `presets save --server generic-ls-bound --ctx 4096` saved; `--preset` gave `--ctx-size 4096 --seed 7 --temp 0.7` with a hand-added `ls-seed` |
| 12 | Hand comment survived `presets save`; `backend.generic` block untouched |
| 13 | After a daemon restart, bare `start` reused `generic-ls-bound` + ctx |
| 14 | TUI picker pre-fills the generic server; `ls-seed` edited to 11 reached argv as `--seed 11` |
| 15 | Running view: `generic`, `ls-ctx 3072`, `ls-temp 0.7`, matching argv |
| 16 | gufo got `--served-model-name Qwen3.8-Flash-Next-UD-Q4_K_XL` (the published id); proxied chat 200. gufo answers a mismatched name with 404 `model_not_found` |
| 17 | TUI `Ctrl+r` restart with gufo loaded: new daemon up, 0 launches. gufo stopped in ~5 s and Halogen (case 21) in 4.2 s, so no real engine exercised the >8 s path of the restart wait |
| 18 | `daemon stop` printed `stopped` only after gufo exited |
| 19 | `--host` / `--api-key` in extras refused, exit 64 |
| 20 | User `config.yaml` hash unchanged; user daemon pid unchanged (rechecked after 21-22) |
| 21 | Ready in 100 s (cold). Container env: `HALOGEN_MODEL_ID=flash-next-halogen`, `HALOGEN_CTX` / `HALOGEN_KV_POOL_POSITIONS=16384`, `HALOGEN_TEMPERATURE=1.0`, `HALOGEN_MTP_DEPTH=3`; port `127.0.0.1:21500` only. Proxied chat 200. `status`: `stop_grace_secs: 90`. TUI `Ctrl+r`: engine and old daemon gone 4.2 s after confirm, new daemon up, container removed |
| 22 | Ready in 115 s. The wrapper `exec`s CIRU's own `llama-server` (same pid) on `127.0.0.1:21500`; env `PORT`, `CONTEXT_SIZE=16384`, `MTP_DEPTH=3` became `-c 16384 --spec-draft-n-max 3`. Proxied chat to `flash-next-ciru` 200 (CIRU answers as its own alias and ignores `model`). `stop` returned in 0.7 s with the engine gone |

One defect found and fixed here (with a regression test that fails without the fix):

- `start --backend llamacpp` after a generic launch still ran the generic entry: the
  last-used `server` overrode the explicit backend, and `--threads 6` was dropped. An
  inherited server of another backend is now dropped. The TUI's Server row cycled to
  the llama.cpp default also runs llama.cpp now.

Notes, not defects:

- `presets save --ctx N` stores `ctx-size: N` even with a generic `--server`; it still
  reaches the entry's `ctx: true` knob at launch.
- Hand-edits to `config.yaml` need a daemon restart (documented).
