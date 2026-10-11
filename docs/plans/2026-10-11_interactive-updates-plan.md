# Interactive dependency updates

## Goal

Add `check-updates -i` and `check-updates -i -w` to select dependency updates, preview exact file changes, and confirm before writing. Keep existing listing and non-interactive upgrade commands unchanged.

## Context

- `src/check_updates/cli.py` collects updates before writing. `_check_project` shares PyPI results through a cache. `_write_updates` currently mutates the TOML document and writes it immediately.
- `src/check_updates/dependencies.py` provides `Dependency`, `Update`, and format-preserving `apply_update`. A dependency already has an array position, but no section label.
- `src/check_updates/workspace.py` provides project discovery and workspace-source exclusions. `Project` stores a path and parsed document, but not the original file bytes.
- `tests/test_cli.py` and `tests/test_workspace.py` use `CliRunner`, mocked PyPI results, and byte comparisons. Extend this offline test pattern.
- The project supports Python 3.10 and later. Typer, packaging, and tomlkit are direct dependencies; no checkbox prompt library is declared.
- `uv.lock` already has uncommitted changes at planning time. Preserve those changes. Do not regenerate or stage the lock file as part of this work.

## Command Contract

| Command | Behavior |
| --- | --- |
| `check-updates` / `check-updates -w` | List updates without prompting or writing. |
| `check-updates -u` / `check-updates -u -w` | Apply all supported updates without prompting, as today. |
| `check-updates -i` / `check-updates -i -w` | Select updates, preview a diff, and confirm selected changes. |
| `check-updates -i -u` | Return usage error 2 before lookups or writes. |

`--interactive` is the long form of `-i`. Interactive mode requires terminal input and output. Without a terminal, return usage error 2 before lookups; do not fall back to automatic updates.

## Interaction Rules

1. Finish file reads and PyPI lookups before opening the selector. If no updates exist, print the existing message and exit without a prompt.
2. Start with no updates selected. Use arrow keys to move, Space to toggle an entry, and Enter to review the selection. Show these controls in the prompt.
3. Group entries by project file. Show the dependency section, original requirement, target version, and version-change label. Each dependency occurrence is a separate selection, including repeated package names in different sections or projects.
4. Provide a project-level action that toggles all updates in that project. A project action selects all if any child is unselected; otherwise it clears all. Derive selected counts from dependency entries, not project actions.
5. Label numeric release-component changes as `major`, `minor`, or `patch`, using the requirement's version as the baseline, not an installed version. Label requirements without a version as `unbounded`; label epoch or other PEP 440 changes as `other`. Labels do not prove compatibility. Use text labels even when color is available.
6. If nothing is selected, exit successfully without writing. Otherwise, show unified diffs for selected changes only, followed by dependency and file counts.
7. Ask `Apply these changes?` with No as the default. No or Escape cancels successfully without writing; Ctrl+C exits with code 130. Confirmation writes only the already-previewed content.
8. Before the first write, compare every affected file with its original bytes. If any file changed during review, return error 1 and write nothing. This check is not protection against a concurrent edit after the check.

## Architecture

- Keep discovery and PyPI lookup rules unchanged. Extend dependency enumeration with section labels only as needed for unambiguous selection.
- Retain original bytes in each loaded project. Prepare selected changes on a fresh parse of that snapshot so preview generation does not mutate the discovery document.
- Use one content-preparation path for diffs and writes. Preserve comments, quotes, markers, extras, and line endings. Diff display must not alter the bytes later written.
- Put prompt controls and selection handling in `src/check_updates/interactive.py`. Keep data preparation and filesystem writes outside that module so tests can replace the prompt boundary.
- Use an existing maintained prompt library rather than a custom terminal event loop. First evaluate Questionary for Python 3.10 support, project toggles, cancellation, and automated keyboard tests. Select one library after this check; do not add alternate UI backends.

## Non-Goals

Version selection, release-note retrieval, dependency resolution, Python-version compatibility checks, prerelease support, new version-constraint handling, external-workspace traversal, and transactional workspace writes are not included. Do not run installers, `uv lock`, or `uv sync` from the CLI. Do not add configuration files or environment variables.

## Plan

- [ ] Verify the prompt library's documented Python support, key bindings, project-toggle support, and cancellation behavior; record the selected library and version in this plan before implementation. Acceptance: documented APIs and a non-interactive keyboard-test feasibility check cover the interaction rules.
- [ ] Add section labels and original-byte snapshots to dependency and project data without changing lookup or workspace behavior. Acceptance: existing tests pass and repeated dependencies have distinct selection identities.
- [ ] Separate content preparation from writing so selected updates produce exact preview and output bytes without early mutations. Acceptance: tests cover selected-only diffs, CRLF, quote styles, comments, markers, and extras.
- [ ] Add the selector with unchecked defaults, project groups and toggles, version-change labels, and explicit cancellation. Acceptance: automated prompt tests cover navigation, Space, Enter, project toggles, Escape, and Ctrl+C without opening a live terminal prompt.
- [ ] Add `-i` / `--interactive`, terminal checks, flag validation, diff display, default-No confirmation, and affected-file snapshot checks. Acceptance: offline CLI tests prove the command contract, confirmation behavior, and zero writes on cancellation or stale input files.
- [ ] Extend workspace tests to select a subset across projects while preserving exclusions, source overrides, and shared lookups. Acceptance: only selected requirement strings change; excluded, unrelated, unselected, and lock files remain byte-identical.
- [ ] Document the commands, controls, version-label limits, terminal requirement, exit codes, and file-write limits in `README.md`. Acceptance: examples match CLI help and tested behavior.
- [ ] Run the checks below and review the final diff. Acceptance: all checks pass and no unrelated files or existing lock-file changes are modified by this work.

## Risks

A terminal library may not support project toggles without custom key bindings. Resolve this in the first task before adding a dependency. Version labels are descriptive only, especially for packages below 1.0.

## Rollback / Recovery

If the feature must be removed, revert its CLI integration, prompt module, and added prompt dependency while retaining existing listing and `-u` behavior. Do not reset unrelated work.

Workspace writes remain sequential. A write failure can leave earlier files updated, as in the current CLI. Report the failing path and retain the list of successfully written files in output. Users can restore affected requirements from the displayed diff or version control; do not automatically reset files or discard existing edits.

## Completion Checklist

- [ ] Existing listing and `-u` behavior passes the regression tests, including workspace behavior.
- [ ] Interactive selection, project toggles, exact diffs, confirmation, cancellation, and repeated dependency entries pass offline tests.
- [ ] Non-terminal input, invalid flag combinations, lookup failures, and files changed during review have the specified exit codes and make no writes.
- [ ] `uv run --frozen --no-sync pytest -q --cov=src --cov-report=term-missing` passes.
- [ ] `uv run --frozen --no-sync ruff check .` and `uv run --frozen --no-sync ruff format --check .` pass.
- [ ] `uv run --frozen --no-sync ty check .` and `git diff --check` pass.
- [ ] CLI help lists `-i` and `--interactive`; README describes the tested workflow and limits.
- [ ] Final diff review confirms that the work preserves existing `uv.lock` changes and adds only intended source, tests, documentation, and the selected prompt dependency.
