# Usage

[README](../README.md)

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

Review the requirement and lockfile changes and test your project after updating. Run `uv sync` yourself when ready to install packages. If you used `--no-lock`, run `uv lock` first when ready to resolve dependencies.

### Options

| Option | Purpose |
| --- | --- |
| `-i`, `--interactive` | Select updates, preview diffs, and confirm before writing |
| `-u`, `--upgrade` | Write all available updates |
| `--no-lock` | Skip automatic locking; requires `-u` or `-i` |
| `-w`, `--workspace` | Check the uv workspace root and all included members |
| `--target patch\|minor\|latest` | Limit updates relative to the requirement baseline; default: `latest` |
| `--include NAME` | Include a package; repeat for multiple packages |
| `--exclude NAME` | Exclude a package; repeat for multiple packages |
| `--runtime` | Select `[project].dependencies` |
| `--group NAME` | Select a dependency group; repeat for multiple groups |
| `--extra NAME` | Select an optional-dependency extra; repeat for multiple extras |
| `--format table\|json` | Use a table (default) or read-only JSON output |
| `--check` | Do not write; return exit code `3` when updates exist |
| `--diff` | Show a read-only unified diff without a table |
| `--index URL` | Use a Simple index; repeat in first-match priority order |
| `--no-cache` | Bypass persistent public PyPI cache reads and writes |
| `--debug` | Write diagnostic messages and error context to stderr |
| `--explain` | Read-only candidate decisions; extended JSON schema 2 |
| `--show-locked` | Read-only locked registry versions; freshness is not checked |
| `--cooldown DURATION` | Require artifact age, such as `7d`, `12h`, or `30m` |
| `--minimal` / `--no-minimal` | Enable minimal requirement edits, or override configuration |
| `--pre` / `--no-pre` | Admit alpha, beta, and RC releases, or override configuration |
| `--include-glob PATTERN` / `--exclude-glob PATTERN` | Filter normalized package names with quoted globs; repeatable |
| `--member NAME` | Select a named editable project under `-w`; repeatable |
| `--to 'NAME==VERSION'` | Select an exact newer candidate; repeatable |
| `--bound 'NAME<VERSION'` | Replace one explicit upper bound; repeatable; writes require verification |
| `--verify` | Resolve the exact selection before writing; requires `-u` or `-i` |
| `--doctor` | Install and test in a temporary project; requires `-u` and `--test-command` |
| `--test-command PROGRAM ARGS...` | Doctor command with literal arguments; put this option last |
| `--version` | Show the native CLI version |
| `--help` | Show command help |

`-i` and `-u` cannot be combined. Both can be used with `-w` and selection options. `--check`, `--diff`, and `--format json` cannot be combined with `-i` or `-u`. `--check` can accompany JSON or diff output; JSON and diff cannot be combined. Without `-i` or `-u`, no project files are changed.

## Update selection

```sh
# Only patch updates for the named packages.
uvx check-updates --target patch --include typer --include rich

# Upgrade a development group but leave one package unchanged.
uvx check-updates -u --group dev --exclude pytest

# Review runtime dependencies and an optional extra together.
uvx check-updates --runtime --extra test --diff
```

`--include`, `--exclude`, group, extra, and member matching uses exact normalized names: case, hyphens, underscores, and dots are equivalent. Original section names stay unchanged in output and TOML. Exact filters do not interpret glob characters. Separate `--include-glob` and `--exclude-glob` options accept quoted glob patterns over normalized names. Exact and glob inclusion filters form a union. Exclusions take precedence over inclusions. Without section options, every supported section is checked. Section options select their union; package filters further restrict that union. Group includes are left unchanged and are not expanded. A nonexistent group, extra, or package filter selects nothing.

`patch` keeps the baseline's major and minor components; `minor` keeps its major component. Both exclude epoch changes. `latest` allows any newer stable version admitted by preserved upper bounds and exclusions. The baseline is the requirement's version, not an installed or locked version. Requirements without a baseline are skipped in patch/minor mode. These labels follow numeric release components, including for packages below version 1.0, and do not guarantee compatibility.

## Interactive updates

```sh
uvx check-updates -i
```

The command reads files and completes index lookups before showing the selector. Entries are grouped by project file and start unchecked. Repeated dependencies can be selected separately.

| Key | Action |
| --- | --- |
| Arrow keys | Move between entries and project actions |
| Space | Toggle an entry; on a project action, select all if any are unchecked, otherwise clear the project |
| Enter | Preview selected updates; exit if nothing is selected |
| Escape | Cancel without writing |
| Ctrl+C | Exit with code `130` |

After selection, the command shows unified diffs, release links, and dependency and file counts. Public PyPI links point to the target release; project release-note links are included when PyPI advertises a safe HTTP(S) `Changelog`, `Release notes`, or `Releases` URL. Configured indexes provide a credential-free package page instead. Links are displayed, not opened automatically. `Apply these changes?` defaults to No. Press `y` to apply or `n` to cancel, or use arrows or Space to change the choice and Enter to confirm. Only selected updates are written.

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

Without `-w`, only the current directory's `pyproject.toml` is checked. Dependencies declared with `workspace = true` or an external workspace path in that file's `[tool.uv.sources]` are still skipped, with a reminder to use `-w` to check members. Parent workspace source definitions are only read with `-w`. Workspace discovery follows [uv workspace configuration](https://docs.astral.sh/uv/concepts/projects/workspaces/#getting-started). Discovery does not run uv. Successful update writes can run `uv lock` as described in [Automatic locking](locking.md#automatic-locking).

## Related documentation

[Update policies](policies.md) · [Locking and verification](locking.md) · [Output and limitations](reference.md)
