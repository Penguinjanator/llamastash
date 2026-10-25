# recipe-e2e.sh

Runs the `RECIPES.md` blocks against real engines. `tests/recipes_load.rs` proves every
block still loads; this proves each one launches its engine and answers a real request.

```bash
scripts/recipe-e2e.sh --list                    # cases and what each one covers
scripts/recipe-e2e.sh lancet whisper_tiny       # run cases, in order
scripts/recipe-e2e.sh --port-base 16200 speaches
```

## What one case does

1. Writes the recipe block, with this machine's real paths, to a throwaway `config.yaml`.
   The presets-only blocks get a `backend.llamacpp.servers` preamble, because an isolated
   daemon has no default `llama-server` and refuses to start without one.
2. Starts a daemon on its own `LLAMASTASH_STATE_DIR` / `_CONFIG_DIR` / `_CACHE_DIR`, its own
   proxy port, and its own `daemon.port_range`. The daemon on the user's ports is never
   touched, and two runs cannot collide through the engine port allocator.
3. `start`s the row, waits for `ready`, and captures the child's argv.
4. Sends one real request through the proxy: chat completion, embedding, `/v1/systemone`,
   `/v1/audio/transcriptions` with a real wav, `/v1/audio/speech`, `/v1/images/generations`,
   a declared `routes:` prefix, or Piper's `/synthesize`.
5. Stops the launch and the daemon, and appends one TSV row (`machine`, case, verdict,
   evidence) to `~/.cache/llamastash-recipe-e2e/results.tsv`. Engine logs stay in the case
   dir next to the row's evidence.

Verdicts: `PASS`, `FAIL`, `SKIP` (engine binary or weights absent, with the missing path),
`ERROR` (the case definition itself broke). `--keep` leaves the launch up for inspection.

## Knobs

| Setting | Effect |
| --- | --- |
| `RECIPE_E2E_BIN` | binary under test, default `target/debug/llamastash` |
| `--port-base N` | first proxy port; each case takes the next one |
| `E2E_ENGINE_START` | first engine port range; each case takes the next 20 |
| `RECIPE_E2E_WAIT` | seconds to wait for a launch, default 600 |
| `REQ_TIMEOUT` | seconds for one request |
| `RECIPE_E2E_ALLOW_HEAVY` | required for the ~80 GB Flash-Next entries, so a run cannot evict whatever the box is already serving |

Paths and model files per host are set at the top of the script. Cases that need something
the host lacks record SKIP, so one run reports the whole coverage picture.

## Two findings baked into the cases

- **Ready is not loaded.** Chatterbox answers `/health` with `model_loaded: false`, and
  LocalAI answers `/readyz` before its TTS and SD backends warm up, so the first media
  request gets a 500. The media checks retry, and a case can also gate on JSON from the
  engine's own port with `READY_PATH` plus `READY_JQ`.
- **Media routes resolve by mode, and the engine names its own model.** whisper.cpp and
  sd.cpp ignore the name, Speaches wants its HF repo id (`Systran/faster-whisper-small`),
  and LocalAI's Kokoro backend wants a Kokoro voice (`af_heart`, not `alloy`). Cases set
  `STT_MODEL` / `TTS_MODEL` / `IMAGE_MODEL` / `TTS_VOICE` for that.
