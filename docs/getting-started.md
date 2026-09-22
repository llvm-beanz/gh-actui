# Getting started

## Prerequisites

- Rust
- GitHub CLI (`gh`)
- An authenticated GitHub CLI session

Authenticate before starting the extension:

```console
gh auth login
```

## Run from source

Pass either a GitHub repository URL or an `OWNER/REPO` name:

```console
cargo run -- https://github.com/llvm/offload-test-suite
cargo run -- llvm/offload-test-suite
```

The extension uses the authenticated `gh api` command to load all GitHub
Actions workflows for the repository before opening the terminal UI.

## Build

```console
cargo build --release
```

The resulting executable is named `gh-actui`, which allows GitHub CLI to expose
it as `gh actui` when installed as an extension.
