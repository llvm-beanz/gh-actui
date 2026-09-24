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
command to discover active GitHub Actions workflows in the background. The
workflow rows appear as soon as discovery completes. Status and 24-hour,
7-day, and 14-day metrics fill in incrementally, with up to four workflow
histories queried concurrently. The bottom status bar displays progress while
that enrichment is running. Current data refreshes automatically every 60
seconds; the interval is shown in the bottom bar and can be changed for the
current session with `:refresh-rate N`. Run history is cached on disk between
sessions so that refreshes mostly issue conditional requests that GitHub does
not charge against the API rate limit; see the [user reference](./user-reference.md)
for the cache location and the `GH_ACTUI_CACHE` override.
When a state file is supplied, its saved global workflow IDs and tab
arrangement are restored. Each tab keeps its own name, workflow subset, filter,
sort, and selection. Workflow names, paths, workflow state, and run status are
queried again from GitHub.

## Build

```console
cargo build --release
```

The resulting executable is named `gh-actui`, which allows GitHub CLI to expose
it as `gh actui` when installed as an extension.
