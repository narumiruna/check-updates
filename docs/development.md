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

## Git hooks

Use prek with `prek.toml` for local Git hooks. If this checkout has an old pre-commit hook, follow [Migrate from pre-commit](#migrate-from-pre-commit) before running `prek install`.

```sh
uv tool install prek
prek install
prek run --all-files
```

### Migrate from pre-commit

A hook installed with `pre-commit install` still reads the removed `.pre-commit-config.yaml`. It can block commits with a missing-config error. A plain `prek install` keeps the old hook as `pre-commit.legacy` in the active hook directory and runs it too, so the error can remain.

Find the active Git hook directory from the checkout root:

```sh
git rev-parse --path-format=absolute --git-path hooks
```

This command accounts for `core.hooksPath` and linked worktrees. Use the returned directory for the checks below. To check the scope of a `core.hooksPath` setting, run `git config --show-scope --get core.hooksPath`.

Choose one method:

1. If pre-commit is still available and `core.hooksPath` is not set, remove any prek shim first, then replace the old hook. `prek uninstall` restores the legacy hook, if present, so `pre-commit uninstall` can remove it:

   ```sh
   prek uninstall
   pre-commit uninstall
   prek install
   ```

2. Replace the old hook with prek:
   - Before using `--force`, check `pre-commit` and any `pre-commit.legacy` file in the directory returned above. Move any required checks to `prek.toml` before you replace the hook. `--force` also removes the legacy hook.
   - If `core.hooksPath` comes from global or system config, stop and review that setting first. In this case, `--force` installs to the repository's default hooks directory and leaves the active shared hook unchanged.

   ```sh
   prek install --force
   ```

## GitHub workflow security

Workflow actions are pinned to full commit SHAs. Keep the version comments with these pins so Dependabot can update them. Each Dependabot ecosystem has a seven-day cooldown for version updates.

Job names use kebab-case. CI cancels older runs for the same pull request or ref. Version-bump, container, and publication workflows serialize runs for the same ref without canceling an active run. Write permissions are limited to the jobs that need them. Checkouts do not persist credentials, and publication jobs disable uv caching.

Validate workflow changes locally without triggering a release:

```sh
zizmor --pedantic .
actionlint
```

## Release authentication

The publish workflow uses PyPI Trusted Publishing, not `PYPI_TOKEN`. Before the next publication, a PyPI project owner must configure a GitHub trusted publisher for `check-updates` with these values:

- Owner: `narumiruna`
- Repository: `check-updates`
- Workflow filename: `publish.yml`
- Environment: leave empty; the workflow does not select a GitHub environment.

Only the `publish` job, named `publish-pypi`, has `id-token: write`. It runs `uv publish --trusted-publishing always`, so publication fails if the trusted publisher is not configured. After a successful publication, remove the unused `PYPI_TOKEN` repository secret and revoke the old PyPI token.

The version-bump workflow still requires `PAT_TOKEN` to push version commits and tags. Pushes made with that token can trigger the publication workflows; the workflow's `GITHUB_TOKEN` needs only read access.

## Release verification handoff

Local validation uses Linux x86-64, uv 0.13.0, local metadata servers, and inert wheel fixtures. After an authorized PR is pushed, maintainers must check the Linux, macOS, and Windows CI jobs, including wheel smoke tests. Non-host process, path, cleanup, and wheel behavior remain unverified until those jobs pass; do not treat local checks as cross-platform release readiness. Doctor remains explicitly unsupported on Windows.

If a release needs provider-specific private-index verification, the index owner must use owner-managed credentials and a non-production project to check lookup, policy enforcement, verification, and recovery. Do not claim verified support for that provider until routing and authentication pass without credential leakage. Do not put credentials in an issue, PR, or chat.

## Related documentation

[Installation](installation.md) · [Architecture](architecture.md) · [Workflows](workflows.md)
