# gh-actui

`gh-actui` is a GitHub CLI extension for monitoring GitHub Actions written in
Rust with an interactive terminal UI powered by [Ratatui](https://ratatui.rs/).

## Prerequisites

- [Rust](https://www.rust-lang.org/tools/install)
- [GitHub CLI](https://cli.github.com/)

## Run locally

```console
cargo run
```

Use the up arrow or `+` to increment the sample counter, the down arrow or `-`
to decrement it, and `q` or Escape to exit.

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
