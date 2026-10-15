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

A monthly workflow (`.github/workflows/update-llama-ui.yml`) runs `update.sh
latest` at 04:00 UTC on the 1st of each month and opens a PR from
`chore/llama-ui-<build>` when upstream moves. Monthly rather than weekly: every
upstream build changes the archive, so a weekly PR would commit a fresh 3.1 MB
blob to git history almost every week. It never merges, so the check below is
what stops a bad bump.
The workflow writes through the `LLAMA_UI_BUMP_TOKEN` repo secret and does
nothing while it is unset; minting it is Step 2 of
`docs/runbooks/release-0.0.1-bootstrap.md`.

### Before merging a bump

The UI reads `/props` fields we fake, and a bump can start reading one we do
not answer. So before merging, run the daemon from the branch and check the
bundled UI by hand: start one UI-less chat launch, open `/ui/`, and send a
message. It should load without a server error and stream an answer.
