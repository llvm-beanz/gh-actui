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

Pass a GitHub repository URL, an `OWNER/REPO` name, or an existing JSON view
state file:

```console
cargo run -- https://github.com/llvm/offload-test-suite
cargo run -- llvm/offload-test-suite
cargo run -- saved-view.json
```

The terminal UI opens immediately, then uses the authenticated `gh api`
command to load GitHub Actions workflows and run status in the background. The
bottom status bar displays the repository being loaded until the data is
ready.
When a state file is supplied, the saved workflow IDs determine which
workflows are displayed. Names, paths, workflow state, and run status are
queried again from GitHub.

## Build

```console
cargo build --release
```

The resulting executable is named `gh-actui`, which allows GitHub CLI to expose
it as `gh actui` when installed as an extension.
