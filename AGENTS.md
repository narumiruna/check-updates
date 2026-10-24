# Repository guidance

## Documentation

- Explain the main idea first, then add necessary details.
- Use accurate, direct technical prose with clear structure, familiar words, and concise sentences.
- Omit repeated or unrelated content.
- Preserve official names and English technical terms.
- Make documented rules specific and verifiable.
  - Use indented sub-items for rules that depend on a parent rule.
- Use lists, tables, and code examples when they make the documentation easier to understand.
- Use Mermaid syntax for diagrams.
- Keep `README.md` focused on installation, common commands, and key safety notes; put detailed user guidance in `docs/` and link it from the README.
- Update the relevant guides when changing CLI options, update policies, output schemas, or write behavior.
- Use `docs/development.md` for build and release procedures, `docs/locking.md` for write and recovery guarantees, and `docs/indexes.md` for index and authentication rules.
- Check relative links and section anchors after moving documentation.

## Code style

- Use Rust for CLI implementation and tests; `pyproject.toml` configures distribution through maturin, not a Python application.
- Keep Rust formatting compatible with `cargo fmt --check` and avoid Clippy warnings under `-D warnings`.

## Commands

- Use Rust 1.92 or newer; CI pins uv 0.13.0 and prepares Python 3.12 for offline resolver fixtures.
- Run `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, and `cargo test --locked` after Rust changes.
- Use `cargo run -- --help` to inspect CLI options and `cargo build --release --locked` to check a release build.
- After packaging changes, run `uv build`, then `uvx --isolated --no-index --find-links dist check-updates --version` to smoke-test the local wheel.
- Keep `Cargo.lock` and `uv.lock` tracked; update them only when required by the requested dependency or version change.

## Boundaries

- Treat `Cargo.toml` as the version source; preserve maturin's dynamic version configuration and keep the bump-version configuration consistent when changing versions.
- Do not run update or Doctor commands against this repository or a real user project as a test; use temporary fixture projects.
- Do not run `uv publish`, `just publish`, release workflows, or version-bump workflows without explicit authorization; these can publish artifacts or create commits and tags.
- Do not commit generated files from `target/` or `dist/`.

## Security

- Preserve the distinction between read-only checks, requirement writes, automatic locking, verification, and Doctor; do not add implicit installation or test execution to normal modes.
- Keep credentials and private response bodies out of logs, reports, test fixtures, and persisted metadata.
- Preserve guarded writes and recovery checks so failed verification or concurrent user edits do not silently overwrite project inputs.
- Do not describe temporary verification or Doctor directories as a security sandbox; Doctor runs project and dependency code with the user's permissions.
- Use local fixture servers and inert artifacts for index tests; provider-specific checks require owner-managed credentials and a non-production project.

## Testing

- Add regression tests with behavior changes; use unit tests in the relevant `src/` module and integration tests in `tests/` for command and process behavior.
- Follow the existing local HTTP server and temporary-project fixture patterns rather than depending on public PyPI for tests.
- Ensure uv and a local Python interpreter are available for verification and Doctor tests; fixtures disable interpreter downloads and use offline resolution.
- Test formatting preservation, unchanged inputs on failure, and recovery when changing requirement or lockfile writes.
- For documentation-only changes, check links, code fences, and whitespace; application tests are not required unless behavior also changes.
- Report checks that were not run and do not claim cross-platform validation until Linux, macOS, and Windows CI, including wheel smoke tests, passes.
