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
and commands. Enter `:q` to exit.

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
