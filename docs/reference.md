# Output and limitations

[README](../README.md)

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

With `--explain` or `--show-locked`, JSON explicitly uses `schema_version: 2`, preserves the `updates` array, and adds `decisions`. Each decision has `file`, original `requirement` (URL credentials/query data redacted), `section`, occurrence `index`, nullable `latest`, `selected`, `prerelease`, and `reason`, a `rejected` array of `{version, reasons}`, and nullable `locked` context. Unsupported requirements also include `detail`. Summary reasons are `blocked`, `minimal`, `no_newer_release`, `unsupported_requirement`, or `workspace`; a selected update has a null summary reason. Candidate reason codes are `upper_bound`, `excluded`, `target`, `python`, `prerelease`, `development`, `yanked`, `cooldown`, `unknown_upload_time`, `local_version`, and `exact_target`. Reasons aggregate artifact failures only when no artifact passes every active rule. Locked context contains `state`, `freshness`, and `versions`; states are `missing`, `malformed`, `unsupported`, `unknown`, `source_mismatch`, and `unverified`. Each listed version includes `version`, package `markers`, and source type `registry`. Plain `--format json` retains schema version 1 exactly.

### Debug diagnostics

Add `--debug` to trace project discovery, dependency checks, and update decisions. Errors include context, not Python tracebacks. Normal stdout output and exit codes are unchanged.

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

## Update scope and limitations

- Checks `[project].dependencies`, `[project.optional-dependencies]`, and `[dependency-groups]`.
- Selects the highest eligible stable release with at least one non-yanked distribution file. Ignores prereleases unless `--pre` is active, and always ignores development releases. Applies target limits and Python metadata filtering before selection. Local versions such as `2.0+internal` are eligible only for exact `==` pins. Bare, `>=`, and `~=` requirements cannot use them.
- Updates single `>=`, `==`, and `~=` version constraints. Also supports one `>=` lower bound combined with `<`/`<=` upper bounds or `!=` exclusions, including wildcard exclusions. Only the lower bound changes; the target must satisfy all preserved bounds and exclusions. Adds `>=VERSION` to bare requirements in `latest` mode. Does not lower existing versions.
- Preserves extras, environment markers, comments, quotes, TOML formatting, and line endings. Checks marked dependencies regardless of the current environment.
- Reports and skips URL dependencies, wildcard pins, upper-bound-only or exclusion-only requirements, strict `>` bounds, and ambiguous composites such as multiple lower bounds or `==`/`~=` combined with other constraints. Dependency-group include entries remain unchanged.

## Python metadata filtering

When `[project].requires-python` exists, the tool uses its lowest admitted final version as a metadata baseline, not the running interpreter. The range must have an inclusive `>=` or `~=` floor; additional `>=`, `~=`, `<`, `<=`, and `!=` clauses are allowed only when that floor is admitted. Unsupported or inconsistent ranges stop the check rather than guessing.

A release is eligible if at least one non-yanked file has a `Requires-Python` range admitting that baseline. Missing metadata remains eligible; malformed ranges are ineligible when filtering is active. Each workspace project uses its own baseline. Without `requires-python`, no Python filtering is performed.

This does **not** prove support for the project's entire Python range, wheel platform availability, transitive dependencies, or runtime compatibility. Update selection does not resolve dependencies. Automatic locking delegates resolution to uv. Review release notes and test before installing updates; resolve manually when automatic locking is skipped.

## Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Completed, including no available updates, no selection, or cancelled confirmation |
| `1` | File, parsing, network, or automatic-lock error |
| `2` | Invalid option value/combination, or interactive mode without terminal input and output |
| `3` | `--check` found updates within the selected scope |
| `130` | Interrupted with Ctrl+C; updates remain if locking had started |

## Related documentation

[CLI options](usage.md#options) · [Locking and verification](locking.md) · [Package indexes](indexes.md)
