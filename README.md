# gh-actui

`gh-actui` is a GitHub CLI extension for monitoring GitHub Actions written in
Rust with an interactive terminal UI powered by [Ratatui](https://ratatui.rs/).

## Prerequisites

- [Rust](https://www.rust-lang.org/tools/install)
- [GitHub CLI](https://cli.github.com/)

## Run locally

```console
cargo run -- https://github.com/llvm/offload-test-suite
```

The table displays the repository's active GitHub Actions workflows with
at-a-glance run status and supports Vim-style keyboard selection, scrolling,
and commands. Multiple tabs can show independently filtered and sorted subsets
of one shared workflow collection. Sessions can be saved with `:w` and restored
with `:e` or by passing the state file on the command line. Saved sessions
contain stable workflow IDs and tab state; current workflow and run data is
refreshed when they are loaded. Enter `:q` to exit.

See [Getting started](docs/getting-started.md) for setup and authentication,
and the [User reference](docs/user-reference.md) for arguments and shortcuts.

## Development

```console
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

GitHub CLI discovers extensions by their `gh-<name>` executable name. The crate
therefore builds a `gh-actui` binary:

```console
cargo build --release
```

Release artifacts can be packaged for supported platforms and installed from a
GitHub repository with:

```console
gh extension install llvm-beanz/gh-actui
```

Pushing a tag matching `v*` runs the release workflow, which builds Linux
amd64, Windows amd64, and macOS amd64/arm64 executables and publishes them with
[`cli/gh-extension-precompile`](https://github.com/cli/gh-extension-precompile).
Tags containing a hyphen, such as `v0.2.0-rc.1`, produce prereleases.
