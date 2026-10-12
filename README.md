# check-updates

Check Python dependencies in `pyproject.toml` for newer stable releases on PyPI or configured package indexes, then choose which updates to apply.

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
| `--debug` | Write detailed diagnostics and error tracebacks to stderr |
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

Package matching uses exact normalized names: case, hyphens, underscores, and dots are equivalent. There are no glob patterns. Exclusions take precedence over inclusions. Without section options, every supported section is checked. Section options select their union; package filters further restrict that union. Group includes are left unchanged and are not expanded. A nonexistent group, extra, or package filter selects nothing.

`patch` keeps the baseline's major and minor components; `minor` keeps its major component. Both exclude epoch changes. `latest` allows any newer stable version admitted by preserved upper bounds and exclusions. The baseline is the requirement's version, not an installed or locked version. Requirements without a baseline are skipped in patch/minor mode. These labels follow numeric release components, including for packages below version 1.0, and do not guarantee compatibility.

## Automation and previews

```sh
# Exit 3 if selected dependencies have updates; exit 0 otherwise.
uvx check-updates --check

# Structured output with the same CI exit-code behavior.
uvx check-updates -w --format json --check

# Preview all selected changes without interactive input or writes.
uvx check-updates --diff --target minor
```

JSON stdout is one object with `schema_version: 1` and an `updates` array. Each occurrence includes `file`, original `requirement`, `replacement`, target `version`, `section` (an array of path components), dependency-array `index`, `change`, `release_url`, and `project_release_url`. Links are nullable. Repeated requirements are separate occurrences. An empty result is `{"schema_version": 1, "updates": []}`. Diagnostics and skip messages go to stderr. Errors return code `1` without a partial JSON result. Diff output is empty when no changes are available.

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

Without `-w`, only the current directory's `pyproject.toml` is checked. Dependencies declared with `workspace = true` or an external workspace path in that file's `[tool.uv.sources]` are still skipped, with a reminder to use `-w` to check members. Parent workspace source definitions are only read with `-w`. Workspace discovery follows [uv workspace configuration](https://docs.astral.sh/uv/concepts/projects/workspaces/#getting-started). It does not run uv.

## Update scope and limitations

- Checks `[project].dependencies`, `[project.optional-dependencies]`, and `[dependency-groups]`.
- Selects the highest eligible stable release with at least one non-yanked distribution file. Ignores prereleases and development releases. Applies target limits and Python metadata filtering before selection. Local versions such as `2.0+internal` are eligible only for exact `==` pins. Bare, `>=`, and `~=` requirements cannot use them.
- Updates single `>=`, `==`, and `~=` version constraints. Also supports one `>=` lower bound combined with `<`/`<=` upper bounds or `!=` exclusions, including wildcard exclusions. Only the lower bound changes; the target must satisfy all preserved bounds and exclusions. Adds `>=VERSION` to bare requirements in `latest` mode. Does not lower existing versions.
- Preserves extras, environment markers, comments, quotes, TOML formatting, and line endings. Checks marked dependencies regardless of the current environment.
- Reports and skips URL dependencies, wildcard pins, upper-bound-only or exclusion-only requirements, strict `>` bounds, and ambiguous composites such as multiple lower bounds or `==`/`~=` combined with other constraints. Dependency-group include entries remain unchanged.

### Python metadata filtering

When `[project].requires-python` exists, the tool uses its lowest admitted final version as a metadata baseline, not the running interpreter. The range must have an inclusive `>=` or `~=` floor; additional `>=`, `~=`, `<`, `<=`, and `!=` clauses are allowed only when that floor is admitted. Unsupported or inconsistent ranges stop the check rather than guessing.

A release is eligible if at least one non-yanked file has a `Requires-Python` range admitting that baseline. Missing metadata remains eligible; malformed ranges are ineligible when filtering is active. Each workspace project uses its own baseline. Without `requires-python`, no Python filtering is performed.

This does **not** prove support for the project's entire Python range, wheel platform availability, transitive dependencies, or runtime compatibility. The tool does not resolve dependencies. Review release notes, resolve, and test before installing updates.

### Package indexes and authentication

Without index configuration, public PyPI's JSON API is used. Configure private indexes using PEP 503 HTML or PEP 691 JSON Simple endpoints:

```sh
uvx check-updates --index https://packages.example.com/simple/
```

Repeated `--index` options override project index configuration and replace the implicit public PyPI fallback. To include public PyPI explicitly, append `--index https://pypi.org/simple/`. Versions come only from the first index that contains the package. A 404 tries the next index; an empty or fully yanked package page does not. Authentication, parsing, and other lookup errors stop the check without writing project files.

Simple indexes must advertise wheel or source-distribution filenames; download files are not fetched. The tool also reads project `[[tool.uv.index]]` and named `[tool.uv.sources]` index assignments:

```toml
[[tool.uv.index]]
name = "internal"
url = "https://packages.example.com/simple/"
explicit = true

[tool.uv.sources]
internal-package = { index = "internal" }
```

With `-w`, root definitions are inherited and member definitions override matching names. Non-explicit indexes are tried in declaration order, with `default = true` indexes last. A default index replaces the implicit PyPI fallback, even when it is also explicit. Explicit indexes are queried only for packages assigned to them. If all eligible indexes return 404, or no index is eligible, a configured default still blocks PyPI. A named source uses only that index and never falls back. Undefined names and marker-specific index assignments are errors; marker-dependent routing is not evaluated. Global uv configuration, pip configuration, keyrings, and index-strategy settings are not read.

Index sources with `extra = "cpu"` apply only to occurrences in `[project.optional-dependencies].cpu`. Each checked occurrence uses its own extra, including when the same package appears in several extras. Extra names use normalized matching. Runtime dependencies and dependency groups do not use extra-qualified sources. If no qualified source matches, normal index selection applies. An unqualified source still applies to every occurrence. Multiple matching index names for one occurrence are an error; marker-specific routing remains unsupported.

For named indexes, supply HTTP Basic credentials using `UV_INDEX_INTERNAL_USERNAME` and `UV_INDEX_INTERNAL_PASSWORD` (uppercase the index name and replace every non-alphanumeric character with an underscore). Alternatively, credentials can be URL-encoded in the URL userinfo; environment credentials override URL credentials. Prefer the named-index environment method so secrets do not appear in shell history or process arguments. The `authenticate` setting accepts `auto` (the default), `always`, or `never`. `auto` uses URL or named environment credentials when present. `always` requires these credentials before a request. `never` rejects URL or named environment credentials before a request and otherwise sends no `Authorization` header. Unknown values are errors. These rules also apply to named sources. No credential providers are used.

Authenticated requests require HTTPS. URLs with queries or fragments are rejected. Cross-origin redirects and HTTPS downgrades are rejected; same-origin redirects retain authentication.

Index errors and debug tracebacks do not expose credentials or private response bodies. Output links omit URL userinfo. Paths and package names can still be sensitive. Private-index metadata is kept only in memory for the invocation and is never written to disk.

A public PyPI 404 stops the check with a message naming the package and suggesting checks for misspellings, private or unpublished packages, and workspace configuration. No files are written when a lookup fails.

### Cache and retries

Public PyPI release metadata is cached for one hour under `$XDG_CACHE_HOME/check-updates`, or `~/.cache/check-updates` when that variable is unset. The cache stores versions, Python constraints, and a safe public release-note link; it does not store credentials or response bodies. Corrupt or expired entries are fetched again. Cache-write failures do not fail the check. Use `--no-cache` for fresh lookups without persistent reads or writes. Per-invocation metadata sharing still applies.

Metadata requests use a 15-second timeout and up to three attempts. Network connection failures and HTTP `408`, `429`, `500`, `502`, `503`, and `504` are retried after 0.25 and 0.5 seconds. Other HTTP errors are not retried. No dependency files are written until all lookups succeed.

All file reads and index lookups finish before writing, so lookup failures do not cause partial updates. Multi-file writes are sequential, not transactional: if a write fails, earlier successful updates remain. Use the output and version control to review or restore changes.

### Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Completed, including no available updates, no selection, or cancelled confirmation |
| `1` | File, parsing, or network error |
| `2` | Invalid option value/combination, or interactive mode without terminal input and output |
| `3` | `--check` found updates within the selected scope |
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
