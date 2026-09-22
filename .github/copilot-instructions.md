# gh-ghui — workspace instructions

Project: Rust Cargo workspace (resolver = "2") providing the `gh actui` GitHub CLI extension and interactive TUI monitoring github actions.

## Structure
- `src` — source code for the CLI and TUI.
- `docs/` — user documentation (markdown); `README.md` links into it.
- `.github/` — project metadata and this file.

## Build & run
- `cargo build` — builds the CLI with the TUI enabled by default.
- `cargo test --all-features` — runs unit, integration, and doc tests for the workspace.

## Documentation
- Keep user-facing documentation synchronized with behavior in the same change; implementation work is not complete while the relevant docs describe old commands, shortcuts, arguments, modes, persistence formats, or workflows.
- Update `docs/user-reference.md` whenever commands, aliases, keyboard shortcuts, modes, dialogs, field behavior, tabs, or saved-session behavior change.
- Update `docs/getting-started.md` when installation, build, authentication, startup, or primary workflow guidance changes.
- Update `README.md` when the project overview, top-level usage examples, or documentation links change.
- Derive command and shortcut documentation from the implemented parser and key handlers. Do not document planned or assumed behavior as available.

## Testing
- Tests are part of the change: when adding or modifying a function, add or update its tests in the same change; do not consider a task complete until `cargo test --all-features` passes.
- Before finishing a change, also run `cargo clippy --all-features --workspace` and `cargo fmt --check`, and fix any findings.
- Unit tests: co-located `#[cfg(test)]` module in the file under test. Name tests by function and scenario (e.g. `parse_owner_repo_missing_slash`).
- Cover error paths and edge cases (bad input, missing token, API failure), not just the happy path.
- Keep tests hermetic and deterministic: no real network access. Put GitHub API access behind an injected client (trait) so tests use a mock; reach for `wiremock`/`httpmock` only when real HTTP semantics matter.
- Structure for testability: keep parsing, formatting, and decision logic in small pure functions; keep `run(...)` a thin adapter over them.
- Test-only dependencies go in `[dev-dependencies]`; if shared, still declare them in `workspace.dependencies` in the root `Cargo.toml`.

## Conventions
- Add new dependencies to `workspace.dependencies` in the root `Cargo.toml` and reference them from crate manifests.
- `Cargo.lock` is intentionally committed (binary workspace) — do not ignore it.
- Keep user-facing docs in `docs/`; avoid duplicating the full TUI reference in `README.md`.
- Revise copilot instructions and documentation as the project evolves to ensure accuracy and relevance.