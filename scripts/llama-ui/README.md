# llama-ui scripts

## `update.sh`

Refreshes the copy of the llama.cpp web UI that ships inside the binary. The
proxy serves that copy at `/ui/` for engines with no UI of their own — see
`docs/usage.md` § Web UI.

```bash
scripts/llama-ui/update.sh latest    # whatever upstream publishes now
scripts/llama-ui/update.sh b11642    # a specific build
```

The script downloads the archive from the ggml-org HuggingFace bucket, refuses
it unless the published `.sha256` matches, reads the real build number out of
`build.json` inside the archive, and writes two files:

| File | What it holds |
|---|---|
| `data/llama-ui/dist.tar.gz` | the archive, `include_bytes!`'d into the binary |
| `data/llama-ui/VERSION` | `build=` and `sha256=` lines for that archive |

Running it again for a build whose sha is already in `VERSION` changes nothing.
Only the script writes `VERSION`; do not edit it by hand.

A weekly workflow (`.github/workflows/update-llama-ui.yml`) runs
`update.sh latest` on a schedule and opens a PR when upstream moves. It is a
no-op until the `LLAMA_UI_BUMP_TOKEN` repo secret exists.

### Before merging a bump

The UI reads `/props` fields we fake, and a bump can start reading one we do
not answer. So before merging, run the daemon from the branch and check the
bundled UI by hand: start one UI-less chat launch, open `/ui/`, and send a
message. It should load without a server error and stream an answer.
