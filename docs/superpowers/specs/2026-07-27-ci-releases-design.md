# CI and Linux Releases Design

## Goal

Add GitHub Actions checks and automated GitHub binary releases for Linux
x86_64 and ARM64.

## Continuous integration

Run on pull requests and pushes to `main`. A single Ubuntu job installs the
GStreamer development packages and stable Rust, then runs:

1. `cargo fmt --all -- --check`
2. `cargo clippy --all-targets -- -D warnings`
3. `cargo build --all-targets --locked`
4. `cargo test --all-targets --locked`

## Release automation

Run `release-plz` on pushes to `main` with two jobs: one creates or updates the
release PR, and one releases only after a release PR is merged. Use git-only
mode so releases are versioned from Git tags and are not published to crates.io.

When `release-plz` creates a GitHub release, two dependent matrix jobs build
`owncast-stream` on native Ubuntu x86_64 and ARM64 runners. Each job installs
the GStreamer development packages, runs `cargo build --release --locked`,
packages the binary as a target-named `.tar.gz`, and uploads it to the release
reported by `release-plz`.

Keeping binary builds in the same workflow avoids requiring a PAT or GitHub App
to trigger a second workflow from a release created with `GITHUB_TOKEN`.

## Files

- `.github/workflows/ci.yml`
- `.github/workflows/release.yml`
- `release-plz.toml`

## Permissions and dependencies

CI receives read-only repository access. Release jobs receive only the
`contents` and `pull-requests` permissions they need. The workflow uses the
repository `GITHUB_TOKEN`; no cargo registry token or additional secret is
required.

Released binaries remain dynamically linked to GStreamer. The release does not
bundle GStreamer or add installers, checksums, signatures, or non-Linux targets.

## Verification

Validate workflow syntax locally, run the four CI commands, and build the local
release binary. The ARM64 build and GitHub release upload are validated by
GitHub Actions because the local host is x86_64.
