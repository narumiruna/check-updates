# check-updates

Check Python dependency versions on PyPI. Run the command in the directory that contains `pyproject.toml`.

```sh
pip install check-updates

# List available updates without changing files.
check-updates

# Apply updates to pyproject.toml.
check-updates -u

# List updates for the uv workspace root and members.
check-updates -w

# Apply updates across the uv workspace.
check-updates -u -w
```

Example output:

```text
"typer>=0.27.1" -> 0.27.3
```

`--upgrade` is the long form of `-u`. `--workspace` is the long form of `-w`.

## Workspaces

With `-w`, the command finds the nearest `pyproject.toml` with `[tool.uv.workspace]` in the current directory or its parents. It checks the root and each declared member, using the `members` and `exclude` glob patterns. Run it from the workspace root, a member, or a member subdirectory.

```toml
[tool.uv.workspace]
members = ["packages/*"]
exclude = ["packages/seeds"]
```

Workspace output identifies each project file. PyPI results are shared across projects. Dependencies marked with `workspace = true` or an external workspace path in `[tool.uv.sources]` are skipped. Root source definitions apply to members unless overridden by a member. External workspaces are not traversed.

If the workspace is missing, a selected member has no `pyproject.toml`, or a file or lookup fails, the command returns exit code 1. All files are read and all lookups finish before any updates are written. File-write failures can leave earlier files updated; multi-file writes are not transactional.

Without `-w`, only the current directory's `pyproject.toml` is checked.

Workspace discovery follows [uv workspace configuration](https://docs.astral.sh/uv/concepts/projects/workspaces/#getting-started). It does not run uv.

## Behavior

- Checks `[project].dependencies`, `[project.optional-dependencies]`, and `[dependency-groups]`.
- Selects the highest stable PyPI release with a non-yanked distribution file. Ignores prereleases and development releases.
- Updates single `>=`, `==`, and `~=` version constraints. Adds `>=VERSION` to requirements without a version constraint. Does not lower existing versions.
- Preserves extras, environment markers, comments, and TOML formatting. Checks marked dependencies regardless of the current environment.
- Reports and skips URL dependencies, wildcard pins, multiple constraints, exclusions, upper bounds, and strict `>` bounds. Dependency-group include entries remain unchanged.
- Returns exit code 1 for file, parsing, or network errors. Completes all lookups before applying updates, so lookup failures do not cause partial edits.

The command does not resolve dependencies or check Python-version compatibility. Review updates before installing them. It reads uv workspace configuration only with `-w`. It does not install packages, change lock files, or run `uv lock` or `uv sync`.

## Development

After installing the project and development dependencies, run checks without updating the environment or lock file:

```sh
uv run --frozen --no-sync pytest
uv run --frozen --no-sync ruff check .
uv run --frozen --no-sync ty check .
```
