# Release 0.0.1 bootstrap — agent-native runbook

Short release-day checklist: [`docs/runbooks/release-0.0.1-checklist.md`](release-0.0.1-checklist.md).

This runbook lists the GitHub org-admin actions that have to happen **before** the first `v0.0.1` tag can produce a working release. Every step has a `gh` CLI primitive a human or an agent can run from the terminal; the web-UI fallback is documented inline for steps that benefit from it.

Companion plan: [`docs/plans/2026-05-19-003-feat-0.2.0-release-setup-plan.md`](../plans/2026-05-19-003-feat-0.2.0-release-setup-plan.md) (filename retains the original `0.2.0` slug for historical record; the actual first release is `0.0.1` per the WIP-versioning decision documented in CHANGELOG).

## Prerequisites

```sh
gh auth status       # must show org membership in llamastash
gh --version         # 2.40+ recommended for the api commands below
```

- Owner of the `llamastash` GitHub organization (or invited as Owner before starting).
- A crates.io account capable of receiving verification email for the publish-update token mint (Step 2). **This is the one step with no CLI primitive** — crates.io has no token-mint API.
- A user account capable of minting a fine-grained PAT under the `llamastash` org. **Also CLI-unavailable** — GitHub has no PAT-mint API.

---

## Step 1 — Create the four repos

| Repo                              | Purpose                              | Visibility | Initial content                      |
| --------------------------------- | ------------------------------------ | ---------- | ------------------------------------ |
| `llamastash/llamastash`           | Main source repo (this one)          | public     | Push `main` from the local checkout  |
| `llamastash/homebrew-llamastash`  | Homebrew tap                         | public     | Push from `../homebrew-llamastash/`  |
| `llamastash/llamastash.github.io` | Marketing site → `llamastash.dev`    | public     | Push from `../llamastash.github.io/` |
| `llamastash/.github`              | Org profile (optional, low priority) | public     | Minimal `profile/README.md`          |

```sh
# Main repo — assumes you're inside the existing checkout.
gh repo create llamastash/llamastash \
  --public \
  --description "Fast, keyboard-driven TUI for launching local llama.cpp models" \
  --source=. --remote=origin --push

# Homebrew tap
cd ../homebrew-llamastash
git init -b main && git add -A && git commit -m "chore: bootstrap tap"
gh repo create llamastash/homebrew-llamastash \
  --public \
  --description "Homebrew tap for llamastash" \
  --source=. --remote=origin --push

# Marketing site
cd ../llamastash.github.io
git init -b main && git add -A && git commit -m "chore: bootstrap site"
gh repo create llamastash/llamastash.github.io \
  --public \
  --description "Marketing site for llamastash — llamastash.dev" \
  --source=. --remote=origin --push
```

The org-profile repo is optional. If you want one:

```sh
mkdir -p ../.github/profile && cd ../.github
git init -b main
printf '# llamastash\n\nA fast TUI for local llama.cpp models.\n' > profile/README.md
git add -A && git commit -m "chore: bootstrap org profile"
gh repo create llamastash/.github --public --source=. --remote=origin --push
```

Verify each push:

```sh
for repo in llamastash homebrew-llamastash llamastash.github.io; do
  gh repo view "llamastash/$repo" --json name,defaultBranchRef \
    --jq '"\(.name) | default=\(.defaultBranchRef.name)"'
done
```

---

## Step 2 — Add secrets

Three secrets live on `llamastash/llamastash`. All of them come from mint flows GitHub and crates.io expose only via the web — set each once, then push via `gh secret set`.

### `CRATES_IO_TOKEN`

Scope: **`publish-update` on crate `llamastash` only.** Do not use a global account token.

1. Open https://crates.io and sign in with the GitHub account that will own the crate.
2. Reserve the crate name by publishing a placeholder version (or use a verification-only token first).
3. **Account → API tokens → New token.** Scope: `publish-update` on crate `llamastash` (per-crate scopes shipped mid-2024).
4. Copy the token.

```sh
gh secret set CRATES_IO_TOKEN --repo llamastash/llamastash --body '<paste-here>'
# or pipe from a password manager:
op read 'op://Personal/crates-io/llamastash-publish-token' \
  | gh secret set CRATES_IO_TOKEN --repo llamastash/llamastash
```

### `GH_BUMP_TOKEN`

Fine-grained PAT used by `release.yml`'s `publish-homebrew` and `publish-site` jobs to clone + commit + push directly into the tap and site repos.

1. **Settings → Developer settings → Personal access tokens → Fine-grained tokens → Generate new token.**
2. **Resource owner:** `llamastash`.
3. **Repository access:** Only select repositories → `homebrew-llamastash`, `llamastash.github.io`.
4. **Permissions:** `Contents: Read and write` only. Do **not** grant `Actions: Read and write` — a leaked token with that scope can fire any workflow in the tap or site repo (including a hostile one a maintainer pushes to a feature branch). The release pipeline does not need it.
5. **Expiration:** 1 year. Set a calendar reminder. Rotation steps are at the bottom of this file.

```sh
gh secret set GH_BUMP_TOKEN --repo llamastash/llamastash --body '<paste-here>'
```

### `LLAMA_UI_BUMP_TOKEN`

Fine-grained PAT used by `update-llama-ui.yml` to push a `chore/llama-ui-<build>` branch and open the UI bump PR on this repo. Until it is set the workflow is a no-op: it prints a run-log notice and exits 0. It cannot be `GITHUB_TOKEN`, because events made with `GITHUB_TOKEN` start no other workflow and `can_approve_pull_request_reviews` is off here, so a bot PR opened by the built-in token would sit with required checks that never run.

1. **Settings → Developer settings → Personal access tokens → Fine-grained tokens → Generate new token.**
2. **Resource owner:** `llamastash`.
3. **Repository access:** Only select repositories → `llamastash`.
4. **Permissions:** `Contents: Read and write` and `Pull requests: Read and write`. Nothing else.
5. **Expiration:** 1 year, on the same reminder as above.

```sh
gh secret set LLAMA_UI_BUMP_TOKEN --repo llamastash/llamastash --body '<paste-here>'

# Confirm all three secrets are set (values are not shown):
gh secret list --repo llamastash/llamastash
```

---

## Step 3 — Configure GitHub Pages on the site repo

Source must be **GitHub Actions** (not "Deploy from a branch"). The deploy workflow calls `actions/configure-pages@v5` with `enablement: true`, which auto-enables Pages on first deploy — but doing it explicitly first removes the 404 window.

```sh
gh api -X POST -H 'Accept: application/vnd.github+json' \
  /repos/llamastash/llamastash.github.io/pages \
  -f 'build_type=workflow' \
  -f 'source[branch]=main' \
  -f 'source[path]=/'
```

Verify:

```sh
gh api /repos/llamastash/llamastash.github.io/pages \
  --jq '"build_type=\(.build_type) | https=\(.https_enforced)"'
```

Custom domain is left empty for now — the `cli.rs` zone-config PR sets it later.

---

## Step 4 — Branch protection (recommended) - TODO

Pre-bootstrap, you'll be pushing direct commits. Apply protection after the first real tag completes; until then the rules will block your own bootstrap pushes.

```sh
# Main repo — require PR review, status checks pass, conversation resolution.
gh api -X PUT -H 'Accept: application/vnd.github+json' \
  /repos/llamastash/llamastash/branches/main/protection \
  -F 'required_pull_request_reviews[required_approving_review_count]=1' \
  -F 'required_pull_request_reviews[dismiss_stale_reviews]=true' \
  -F 'required_status_checks[strict]=true' \
  -F 'required_status_checks[contexts][]=ci' \
  -F 'enforce_admins=false' \
  -F 'required_conversation_resolution=true' \
  -F 'allow_force_pushes=false' \
  -F 'allow_deletions=false'

# Tap + site — required PR review on any workflow change. The bump.yml
# fallback workflows + deploy.yml are the trust boundary for GH_BUMP_TOKEN,
# so workflow file edits must be human-reviewed.
for repo in homebrew-llamastash llamastash.github.io; do
  gh api -X PUT -H 'Accept: application/vnd.github+json' \
    "/repos/llamastash/$repo/branches/main/protection" \
    -F 'required_pull_request_reviews[required_approving_review_count]=1' \
    -F 'allow_force_pushes=false' \
    -F 'allow_deletions=false'
done
```

Add a CODEOWNERS file in each downstream repo so workflow changes require maintainer review:

```sh
for repo in homebrew-llamastash llamastash.github.io; do
  cd "../$repo"
  printf '* @deepu105\n.github/workflows/* @deepu105\n' > CODEOWNERS
  git add CODEOWNERS && git commit -m "chore: add CODEOWNERS" && git push
  cd -
done
```

---

## Step 5 — Dry-run the release pipeline with a prerelease tag that matches `Cargo.toml`

> **2026-05-27**: First successful dry-run completed on run [`26503306323`](https://github.com/llamastash/llamastash/actions/runs/26503306323) — 23-asset release published at `v0.0.1-rc1`, all three downstream `publish-*` jobs correctly skipped, tap and site `main` untouched. The three issues that surfaced and were fixed during the dry-run cycle are captured in [Troubleshooting](#troubleshooting-step-5) at the bottom of this section.

Pre-release tags (`vX.Y.Z-<suffix>`) exercise the upstream half of the pipeline only: `create-release` → `build` → `publish-shasums`. The `publish-homebrew`, `publish-site`, and `publish-cargo` jobs all gate on `is_prerelease == 'false'` and are skipped. **This is intentional** — it means the dry run never writes to the tap, site, or crates.io, so cleanup after the dry run is just deleting the tag and the test release.

Important: `prepare-release` requires the dry-run tag to share the same `major.minor.patch` as `Cargo.toml`. With `Cargo.toml` now at `0.0.1`, the dry-run tag should be `v0.0.1-rc1`.

```sh
# From a throwaway branch in the main repo:
git checkout -b release-dry-run
git tag v0.0.1-rc1
git push origin release-dry-run v0.0.1-rc1

# Watch the run live (blocks until completion):
gh run list --repo llamastash/llamastash --workflow=release.yml --limit 1 \
  --json databaseId --jq '.[0].databaseId' \
  | xargs -I {} gh run watch --repo llamastash/llamastash --exit-status {}

# Verify what landed:
gh release view v0.0.1-rc1 --repo llamastash/llamastash \
  --json assets --jq '.assets[].name | "  " + .'
# Expect: 10 tarballs, 10 .sha256 sidecars, SHA256SUMS, install.sh, install.sh.sha256.

# Verify nothing was written downstream (publish-homebrew / -site / -cargo
# should all be skipped):
gh api /repos/llamastash/homebrew-llamastash/commits/main \
  --jq '.commit.message'   # must NOT mention v0.0.1-rc1
gh api /repos/llamastash/llamastash.github.io/commits/main \
  --jq '.commit.message'   # must NOT mention v0.0.1-rc1
```

Cleanup:

```sh
gh release delete v0.0.1-rc1 --repo llamastash/llamastash --yes --cleanup-tag
git push origin --delete release-dry-run
git branch -D release-dry-run
```

### Troubleshooting (Step 5)

Three failure modes hit during the first dry-run cycle on 2026-05-27. All three are now permanently addressed in code; this list exists so the same diagnosis doesn't get repeated next cycle.

1. **`release-gate (linux-cpu-only)` fails at UAT `init` with "no GH Releases asset matches this hardware (os=Linux, arch=X86_64)".**
   Upstream `ggml-org/llama.cpp` sometimes publishes an incomplete asset matrix (observed on tag `b9352`, which shipped 8 assets instead of the usual 22–29, missing the Linux/Windows variants entirely — filed as [ggml-org/llama.cpp#23771](https://github.com/ggml-org/llama.cpp/issues/23771)). The original `fetch_latest_asset` in `src/init/install/gh_releases.rs` only looked at the single most-recent release; an incomplete latest wedged the dry-run gate.
   *Fix in place*: `fetch_latest_asset` now queries `?per_page=10` and walks back to the newest release that contains a matching asset, logging which releases got skipped. macOS goes through brew so it never hit this. No action needed unless upstream's 10 most-recent releases are all incomplete (which would be a real regression worth investigating).

2. **`release-gate (macos-cpu-only)` fails at `tests/start_model_ipc_test.rs:545` with "call 1 last_params.threads never persisted".**
   The IPC test's state-file persistence poll loops used 3–5 s deadlines, which flaked on macOS-arm64 GH-hosted runners under cargo test parallel load.
   *Fix in place*: bumped to 20 s in commit [`e5e4304`](https://github.com/llamastash/llamastash/commit/e5e4304). Watch for it if the same panic message returns on macOS — a 20 s timeout is generous, so a fresh failure here likely means a real bug in the supervisor's recorder path, not flake.

3. **`build <target>-unknown-linux-musl` fails with `linker '<target>-musl-gcc' not found`.**
   The release build step originally branched on `matrix.use_cross` and ran `cross build` for cross-compiled targets. `cross 0.2.5` detects the env vars that `taiki-e/setup-cross-toolchain-action` exports and falls back to plain `cargo` on the host — which has no musl linker, so the four musl matrix entries failed.
   *Fix in place*: commit [`3ea0f63`](https://github.com/llamastash/llamastash/commit/3ea0f63) drops the `cross build` branch and always runs `cargo build`, matching the kdash pattern this workflow is descended from. `install-action@cross` is still installed (for ad-hoc debugging) but never invoked from the script.

If a future failure doesn't match one of those three patterns, follow the standard diagnosis path: `gh run view <id> --log-failed | head -200`, then trace the panicking line back to source. Most release-pipeline failure modes are catalogued in `docs/plans/2026-05-19-003-feat-0.2.0-release-setup-plan.md` §Risks & Dependencies.

---

## Step 6 — Real release

Only after Step 5 succeeds:

```sh
# 1. Confirm Cargo.toml + CHANGELOG agree (the release-readiness CI job and
#    create-release both verify this — these are local belt-and-suspenders).
grep '^version' Cargo.toml                # version = "0.0.1"
grep -E '^## \[0\.0\.1\]' CHANGELOG.md    # ## [0.0.1] — <date>

# 2. Tag and push.
git tag v0.0.1
git push origin v0.0.1

# 3. Watch the full pipeline (10-15 min on cold caches).
gh run list --repo llamastash/llamastash --workflow=release.yml --limit 1 \
  --json databaseId --jq '.[0].databaseId' \
  | xargs -I {} gh run watch --repo llamastash/llamastash --exit-status {}

# 4. Verify each channel:
gh release view v0.0.1 --repo llamastash/llamastash --web
gh api /repos/llamastash/homebrew-llamastash/commits/main \
  --jq '.commit.message'    # mentions v0.0.1
gh api /repos/llamastash/llamastash.github.io/commits/main \
  --jq '.commit.message'    # mentions v0.0.1

# 5. Fresh-box smoke (Ubuntu container + macOS VM):
docker run --rm -it ubuntu:24.04 bash -c '
  apt-get update && apt-get install -y curl
  curl -fsSL https://llamastash.dev/install.sh | sh
  ~/.local/bin/llamastash --version
'
# macOS smoke: cargo install llamastash, brew install llamastash/llamastash/llamastash,
# curl -fsSL https://llamastash.dev/install.sh | sh — all three.
```

If anything in the post-`publish-cargo` chain fails (rare):

```sh
# Re-run a single failed job from the Actions UI, or via:
gh run rerun --repo llamastash/llamastash --failed <run-id>
```

---

## Token rotation cadence

| Secret            | Trigger to rotate                       | Default cadence | Rotation primitive                                                                         |
| ----------------- | --------------------------------------- | --------------- | ------------------------------------------------------------------------------------------ |
| `CRATES_IO_TOKEN` | First publish, suspected leak, annually | annual          | crates.io UI → `gh secret set CRATES_IO_TOKEN --repo llamastash/llamastash --body '<new>'` |
| `GH_BUMP_TOKEN`   | PAT expiry, leak, annually              | annual          | GitHub PAT UI → `gh secret set GH_BUMP_TOKEN --repo llamastash/llamastash --body '<new>'`  |
| `LLAMA_UI_BUMP_TOKEN` | PAT expiry, leak, annually          | annual          | GitHub PAT UI → `gh secret set LLAMA_UI_BUMP_TOKEN --repo llamastash/llamastash --body '<new>'` |

The long-term answer (tracked in `TODO.md`) is migrating to a scoped GitHub App with OIDC instead of PATs — eliminates rotation entirely. Out of scope for 0.0.1.

To monitor PAT expiry without waiting for the first failed release, set a calendar reminder for ~30 days before the configured expiry. GitHub does not yet expose token expiration via API for fine-grained PATs.
