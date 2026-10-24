# Development

[README](../README.md)

Install Rust 1.92 or newer, uv 0.13.0, and a locally available Python interpreter for offline fixture generation (`uv python install 3.12`). The CLI implementation and tests are Rust; uv is used to build and test PyPI distributions.

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo run -- --help
cargo build --release --locked

# Build a source distribution and a local platform wheel.
uv build

# Test the wheel without reading PyPI.
uvx --isolated --no-index --find-links dist check-updates --version
```

`Cargo.toml` is the version source. `pyproject.toml` reads the version through maturin. Keep `Cargo.lock` and `uv.lock` in version control. The publish workflow builds and tests each platform wheel before publication. Cross-platform builds must pass in CI before release.

The Docker image contains the native CLI and uv for automatic locking. It does not require Python to run the CLI. Source installs and `uv build` compile Rust; platform wheel installs do not.

### Release verification handoff

Local validation uses Linux x86-64, uv 0.13.0, local metadata servers, and inert wheel fixtures. After an authorized PR is pushed, maintainers must check the Linux, macOS, and Windows CI jobs, including wheel smoke tests. Non-host process, path, cleanup, and wheel behavior remain unverified until those jobs pass; do not treat local checks as cross-platform release readiness. Doctor remains explicitly unsupported on Windows.

If a release needs provider-specific private-index verification, the index owner must use owner-managed credentials and a non-production project to check lookup, policy enforcement, verification, and recovery. Do not claim verified support for that provider until routing and authentication pass without credential leakage. Do not put credentials in an issue, PR, or chat.

## Related documentation

[Installation](installation.md)
