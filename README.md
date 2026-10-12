# check-updates

Check Python dependencies in `pyproject.toml` for newer stable releases on PyPI, then choose which updates to apply.

Run with `uvx`, provided by [`uv`](https://docs.astral.sh/uv/getting-started/installation/), without installing check-updates into your project environment. The tool only changes dependency requirements. It does not install packages, change lock files, or run `uv lock` or `uv sync`.

## Quick start

Run these commands in the directory that contains `pyproject.toml`:

```sh
# List available updates without changing files.
uvx check-updates

# Select updates, preview diffs, and confirm before writing.
uvx check-updates -i

# Apply all available updates without confirmation.
uvx check-updates -u
```

Review the changes and test your project after updating. If you manage the project with uv, run `uv lock` or `uv sync` yourself when ready.

### Options

| Option | Purpose |
| --- | --- |
| `-i`, `--interactive` | Select updates, preview diffs, and confirm before writing |
| `-u`, `--upgrade` | Write all available updates |
| `-w`, `--workspace` | Check the uv workspace root and all included members |
| `--debug` | Write detailed diagnostics and error tracebacks to stderr |
| `--help` | Show command help |

`-i` and `-u` cannot be combined. Both can be used with `-w`. Without `-i` or `-u`, the command only lists updates.

### Debug diagnostics

Add `--debug` to any mode to trace project discovery, workspace member selection, dependency checks and skip reasons, PyPI requests and release filtering, cache hits, update decisions, interactive selection and confirmation, and file writes. Errors include tracebacks. Normal stdout output and exit codes are unchanged.

```sh
uvx check-updates --debug -w 2>debug.log
```

Diagnostics do not dump project contents, request headers, or PyPI response bodies. Paths, package names, and exception messages can still contain sensitive information; review logs before sharing them.

### Example output

```text
Dependency updates

pyproject.toml:
╭─────────────────────┬────────┬────────┬──────────────────────╮
│ Current requirement │ Target │ Change │ Section              │
├─────────────────────┼────────┼────────┼──────────────────────┤
│ typer>=0.27.1       │ 0.27.3 │ patch  │ project.dependencies │
╰─────────────────────┴────────┴────────┴──────────────────────╯

1 dependency with updates in 1 file.
Change labels compare requirement versions, not installed versions or compatibility.
No files changed. Use -u to apply all updates or -i to select updates.
```

Tables group updates by project file and show the original requirement, target version, change label, and dependency section. Long requirements wrap instead of being truncated. Terminal output uses colors; redirected output has no ANSI color codes.

`major`, `minor`, and `patch` compare release components against the requirement's version, not an installed version. `unbounded` means no version was specified. Epoch changes and other PEP 440 forms use `other`. These labels do not guarantee compatibility, including for packages below version 1.0.

## Interactive updates

```sh
uvx check-updates -i
```

The command reads files and completes PyPI lookups before showing the selector. Entries are grouped by project file and start unchecked. Repeated dependencies can be selected separately.

| Key | Action |
| --- | --- |
| Arrow keys | Move between entries and project actions |
| Space | Toggle an entry; on a project action, select all if any are unchecked, otherwise clear the project |
| Enter | Preview selected updates; exit if nothing is selected |
| Escape | Cancel without writing |
| Ctrl+C | Exit with code `130` |

After selection, the command shows unified diffs and dependency and file counts. `Apply these changes?` defaults to No. Press `y` to apply or `n` to cancel, or use arrows or Space to change the choice and Enter to confirm. Only selected updates are written.

Interactive mode requires terminal input and output. For automation, use the non-interactive `uvx check-updates` or `uvx check-updates -u`.

Before writing, the command checks that every affected file still matches its original content. If a file was edited, deleted, or became unreadable during review, it stops without writing any files. This check does not prevent concurrent edits after the check.

## uv workspaces

Add `-w` to check the workspace root and all included members:

```sh
# List updates across the workspace.
uvx check-updates -w

# Select and confirm workspace updates.
uvx check-updates -i -w

# Apply all available workspace updates.
uvx check-updates -u -w
```

Run from the workspace root, a member, or a member subdirectory. The command searches the current directory and its parents for the nearest `pyproject.toml` with `[tool.uv.workspace]`, then selects projects using the `members` and `exclude` glob patterns:

```toml
[tool.uv.workspace]
members = ["packages/*"]
exclude = ["packages/seeds"]
```

- Output identifies each project file. PyPI lookup results are shared across projects.
- Dependencies marked with `workspace = true` or an external workspace path in `[tool.uv.sources]` are skipped. External workspaces are not traversed.
- Root source definitions apply to members unless overridden by a member.
- A missing workspace, a selected member without `pyproject.toml`, or a read or lookup failure stops the command with exit code `1`.

Without `-w`, only the current directory's `pyproject.toml` is checked. Workspace discovery follows [uv workspace configuration](https://docs.astral.sh/uv/concepts/projects/workspaces/#getting-started). It does not run uv.

## Update scope and limitations

- Checks `[project].dependencies`, `[project.optional-dependencies]`, and `[dependency-groups]`.
- Selects the highest stable PyPI release with at least one non-yanked distribution file. Ignores prereleases and development releases.
- Updates single `>=`, `==`, and `~=` version constraints. Adds `>=VERSION` to requirements without a version constraint. Does not lower existing versions.
- Preserves extras, environment markers, comments, quotes, TOML formatting, and line endings. Checks marked dependencies regardless of the current environment.
- Reports and skips URL dependencies, wildcard pins, multiple constraints, exclusions, upper bounds, and strict `>` bounds. Dependency-group include entries remain unchanged.

The tool does not resolve dependencies or check Python-version compatibility. Check compatibility before installing updates.

All file reads and PyPI lookups finish before writing, so lookup failures do not cause partial updates. Multi-file writes are sequential, not transactional: if a write fails, earlier successful updates remain. Use the output and version control to review or restore changes.

### Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Completed, including no available updates, no selection, or cancelled confirmation |
| `1` | File, parsing, or network error |
| `2` | Invalid option combination, or interactive mode without terminal input and output |
| `130` | Interrupted with Ctrl+C |

## Development

Set up the development environment from the repository:

```sh
uv sync --frozen
```

Run checks without updating the environment or lock file:

```sh
uv run --frozen --no-sync pytest
uv run --frozen --no-sync ruff check .
uv run --frozen --no-sync ty check .
```
