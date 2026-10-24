# check-updates

Check Python dependencies in `pyproject.toml` for newer releases, then choose which updates to apply. Supports PyPI, private package indexes, dependency groups, optional extras, and uv workspaces.

## Quick start

Install [uv](https://docs.astral.sh/uv/getting-started/installation/), then run these commands in the directory that contains `pyproject.toml`:

```sh
# List available updates without changing files.
uvx check-updates

# Select updates, preview diffs, and confirm before writing.
uvx check-updates -i

# Apply all available updates without confirmation.
uvx check-updates -u
```

After writing updates, the command runs `uv lock` if the selected project root already has `uv.lock`. Use `--no-lock` with `-u` or `-i` to update requirements only. Lock failures leave requirement changes in place; review the files and run `uv lock` after correcting the error.

Normal modes do not install packages or run `uv sync`. Review the changes and test your project. Run `uv sync` when ready to install packages. See [locking and verification](docs/locking.md) for details.

## Installation

`uvx` runs the command without installing it into your project environment. To install a persistent command:

```sh
uv tool install check-updates

# Or use pip.
pip install check-updates
```

PyPI installation requires Python 3.10 or newer. Platform wheels contain a native Rust executable and do not require Rust. Source installs require Rust and a C compiler. See [supported platforms and installation details](docs/installation.md).

## Common commands

```sh
# Check all projects in a uv workspace.
uvx check-updates -w

# Preview patch updates only.
uvx check-updates --target patch --diff

# Update a development group, except pytest.
uvx check-updates -u --group dev --exclude pytest

# Return exit code 3 if updates are available; do not change files.
uvx check-updates --check

# Show all options.
uvx check-updates --help
```

## What to expect

- Checks runtime dependencies, optional extras, and dependency groups by default.
- Selects stable releases by default and preserves supported upper bounds and exclusions.
- Preserves TOML formatting, comments, extras, and environment markers.
- Skips unsupported requirements, such as direct URLs and wildcard pins.
- Compares declared requirements, not installed versions. An available update does not prove compatibility.

Optional [verification and Doctor modes](docs/locking.md#verified-updates) can resolve updates before writing. Doctor also installs dependencies and runs your test command in a temporary project. It executes code with your permissions; it is not a security sandbox.

## Documentation

| Guide | Contents |
| --- | --- |
| [Installation](docs/installation.md) | Supported platforms, Python requirements, and Cargo installation |
| [Usage](docs/usage.md) | CLI options, package selection, interactive controls, and workspaces |
| [Update policies](docs/policies.md) | Saved configuration, cooldowns, prereleases, and decision reports |
| [Locking and verification](docs/locking.md) | Automatic locking, verified updates, Doctor, and recovery |
| [Output and limitations](docs/reference.md) | JSON schemas, CI, exit codes, and supported requirements |
| [Package indexes](docs/indexes.md) | Private indexes, authentication, caching, and retries |
| [Development](docs/development.md) | Build, test, and release checks |
