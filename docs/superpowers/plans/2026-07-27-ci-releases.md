# CI and Linux Releases Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add GitHub Actions checks and automated GitHub binary releases for Linux x86_64 and ARM64.

**Architecture:** A CI workflow runs the four requested Cargo checks on pushes and pull requests. A release workflow lets release-plz manage git-only release PRs, tags, and GitHub releases, then builds and uploads two native Linux binaries in the same workflow so no extra token is needed.

**Tech Stack:** GitHub Actions, Cargo, release-plz, Ubuntu x86_64 and ARM64 hosted runners, GStreamer

## Global Constraints

- Publish GitHub releases only; do not publish to crates.io.
- Build Linux x86_64 and ARM64 binaries on native runners.
- Use only `GITHUB_TOKEN`; require no additional repository secret.
- Keep binaries dynamically linked to GStreamer.
- Do not add installers, checksums, signatures, caches, or non-Linux targets.

---

### Task 1: Continuous integration workflow

**Files:**
- Create: `.github/workflows/ci.yml`

**Interfaces:**
- Consumes: `Cargo.toml`, `Cargo.lock`, and the existing Rust source and tests
- Produces: GitHub checks named `Format`, `Clippy`, `Compile`, and `Test`

- [ ] **Step 1: Create the workflow**

```yaml
name: CI

on:
  push:
    branches: [main]
  pull_request:

permissions:
  contents: read

jobs:
  checks:
    runs-on: ubuntu-24.04
    steps:
      - uses: actions/checkout@v6
      - name: Install GStreamer
        run: |
          sudo apt-get update
          sudo apt-get install -y \
            libgstreamer1.0-dev \
            libgstreamer-plugins-base1.0-dev \
            gstreamer1.0-plugins-base \
            gstreamer1.0-plugins-good \
            gstreamer1.0-plugins-bad \
            gstreamer1.0-plugins-ugly \
            gstreamer1.0-libav
      - name: Install Rust
        uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt, clippy
      - name: Format
        run: cargo fmt --all -- --check
      - name: Clippy
        run: cargo clippy --all-targets --locked -- -D warnings
      - name: Compile
        run: cargo build --all-targets --locked
      - name: Test
        run: cargo test --all-targets --locked
```

- [ ] **Step 2: Validate the workflow and commands**

Run:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo build --all-targets --locked
cargo test --all-targets --locked
```

Expected: every command exits with status 0.

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/ci.yml
git commit -m "ci: add Rust checks"
```

### Task 2: Git-only release-plz automation

**Files:**
- Create: `release-plz.toml`
- Create: `.github/workflows/release.yml`

**Interfaces:**
- Consumes: merges to `main`, release-plz release output, and `GITHUB_TOKEN`
- Produces: release PRs, `v<version>` tags, GitHub releases, and two `.tar.gz` release assets

- [ ] **Step 1: Configure git-only releases**

```toml
[workspace]
git_only = true
release_always = false
```

- [ ] **Step 2: Create the release workflow**

```yaml
name: Release

on:
  push:
    branches: [main]

jobs:
  release:
    runs-on: ubuntu-24.04
    permissions:
      contents: write
      pull-requests: read
    outputs:
      created: ${{ steps.release.outputs.releases_created }}
      tag: ${{ steps.metadata.outputs.tag }}
    steps:
      - uses: actions/checkout@v6
        with:
          fetch-depth: 0
          persist-credentials: false
      - uses: dtolnay/rust-toolchain@stable
      - id: release
        uses: release-plz/action@v0.3
        with:
          command: release
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
      - id: metadata
        if: steps.release.outputs.releases_created == 'true'
        env:
          RELEASES: ${{ steps.release.outputs.releases }}
        run: echo "tag=$(jq -r '.[0].tag' <<<"$RELEASES")" >> "$GITHUB_OUTPUT"

  release-pr:
    runs-on: ubuntu-24.04
    permissions:
      contents: write
      pull-requests: write
    concurrency:
      group: release-plz-${{ github.ref }}
      cancel-in-progress: false
    steps:
      - uses: actions/checkout@v6
        with:
          fetch-depth: 0
          persist-credentials: false
      - uses: dtolnay/rust-toolchain@stable
      - uses: release-plz/action@v0.3
        with:
          command: release-pr
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}

  binaries:
    needs: release
    if: needs.release.outputs.created == 'true'
    strategy:
      matrix:
        include:
          - runner: ubuntu-24.04
            asset: owncast-stream-linux-amd64.tar.gz
          - runner: ubuntu-24.04-arm
            asset: owncast-stream-linux-arm64.tar.gz
    runs-on: ${{ matrix.runner }}
    permissions:
      contents: write
    steps:
      - uses: actions/checkout@v6
      - name: Install GStreamer
        run: |
          sudo apt-get update
          sudo apt-get install -y \
            libgstreamer1.0-dev \
            libgstreamer-plugins-base1.0-dev
      - uses: dtolnay/rust-toolchain@stable
      - run: cargo build --release --locked
      - name: Package binary
        env:
          ASSET: ${{ matrix.asset }}
        run: tar -C target/release -czf "$ASSET" owncast-stream
      - name: Upload binary
        env:
          GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          RELEASE_TAG: ${{ needs.release.outputs.tag }}
          ASSET: ${{ matrix.asset }}
        run: gh release upload "$RELEASE_TAG" "$ASSET" --clobber
```

- [ ] **Step 3: Validate configuration and the local release build**

Run:

```bash
cargo build --release --locked
test -x target/release/owncast-stream
git diff --check
```

Expected: the build succeeds, the binary exists, and the diff has no whitespace errors.

- [ ] **Step 4: Commit**

```bash
git add release-plz.toml .github/workflows/release.yml
git commit -m "ci: automate Linux releases"
```

### Task 3: Final verification

**Files:**
- Verify: `.github/workflows/ci.yml`
- Verify: `.github/workflows/release.yml`
- Verify: `release-plz.toml`

**Interfaces:**
- Consumes: all files from Tasks 1 and 2
- Produces: a review-ready branch with locally verified workflows and build commands

- [ ] **Step 1: Inspect exact workflow behavior**

Run:

```bash
git diff origin/main...HEAD -- .github release-plz.toml
```

Expected: only the approved CI and release behavior is present.

- [ ] **Step 2: Run the full local gate**

Run:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo build --all-targets --locked
cargo test --all-targets --locked
cargo build --release --locked
git diff --check
```

Expected: all commands exit with status 0.
