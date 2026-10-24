# Locking, verification, and Doctor

[README](../README.md)

## Automatic locking

In default mode, after `-u` or confirmed `-i` writes updates, the command runs plain `uv lock` once in the selected project root if that directory already has a regular `uv.lock` file. No lockfile is created when one is absent. Read-only modes, no updates, empty selection, cancellation, and lookup, snapshot, or write failures do not start default automatic locking. Verified modes resolve before writing, as described in [Verified updates](#verified-updates).

```sh
# Update requirements only, without running uv.
uvx check-updates -u --no-lock
```

With `-w`, locking runs at the discovered workspace root, even if only a member changes. Without `-w`, only the current project directory is inspected for `uv.lock`; a parent workspace lockfile is not selected. If a local lockfile exists, the command also checks parent `pyproject.toml` files for a workspace. When a parent workspace exists, automatic locking is skipped, even if the current project is excluded from it. Requirement updates remain, and a message gives recovery steps. A workspace declared in the current project stops this parent check. Use `-w` to update a workspace's shared lockfile from a member.

Locking requires `uv` on `PATH` and can access the network. It uses uv's normal configuration and inherited environment, except `UV_PROJECT` and `UV_FROZEN`. These two variables are removed from the child environment so uv locks the selected project and does not skip resolution. Credentials and other configuration variables are kept. Lookup options such as `--index`, selection options, and `--no-cache` are not forwarded to uv. Configure uv separately if lock resolution needs a different index. Child input is closed, so authentication cannot prompt; configure credentials before running the command.

Plain `uv lock` does not request a full upgrade with `--upgrade`, but resolution can still change transitive dependencies. It does not sync the project environment. uv diagnostics are shown directly and are not sanitized by check-updates; review them before sharing.

If uv is missing, cannot start, or exits with an error, check-updates returns `1`. Requirement changes remain, and `uv.lock` may be stale or changed. Correct the cause and run `uv lock` in the reported root directory, or review and restore the affected files using version control. An interrupt during locking returns `130`; requirement changes remain and lock completion is not confirmed. Neither failure automatically restores files.

## Verified updates

```sh
uvx check-updates -i --verify
uvx check-updates -u --verify

# Explicitly replace one supported upper bound; inspect the complete diff.
uvx check-updates --bound 'example-package<3' --diff
uvx check-updates -i --bound 'example-package<3' --to 'example-package==2.4.1'
```

`--verify` requires `-u` or `-i`, an existing regular `uv.lock`, and uv on `PATH`. It conflicts with `--no-lock`. Bounds also require the verified path when written. `--bound` initially supports one unambiguous explicit `<` or `<=` upper bound in a supported `>=` composite. It preserves exclusions and source formatting. It rejects implicit `~=` bounds, upper-bound-only requirements, and multiple upper bounds. No next-major upper bound is inferred. Read-only previews need no uv; writing shows the complete change. With `--minimal`, a bound edit can preserve the old lower bound while verification still resolves the exact selected candidate, including a `--to` target.

Verification creates a temporary project, resolves the exact selected versions, validates a relocatable lockfile, then checks all original resolver inputs before application. Interactive mode shows successful resolution before final confirmation. A permissive `>=` requirement cannot pass verification merely by retaining an older locked version. Different selected versions of the same package under different markers are currently refused. Successful resolution does not prove runtime compatibility.

The tested resolver is uv `0.13.0`; required flags are checked at runtime. Supported inputs are static virtual projects/workspaces without a `build-system` or dynamic metadata. Workspace-relative layout, local `workspace = true` sources, root-configured named indexes, and absolute wheel directories are supported. Resolver/index settings must be at the workspace root; member source mappings and `package = false` remain supported. Only `first-index` routing is accepted, and authenticated indexes require HTTPS. Relative/external path, Git, direct URL, editable/build projects, symlink inputs, `uv.toml`, inherited user/system uv configuration, unsupported `tool.uv` keys, and unrecognized resolver `UV_*` overrides are refused rather than silently changed. `UV_PROJECT` and `UV_FROZEN` are removed; named-index environment credentials and the interpreter installation directory remain available. Interpreter downloads and keyring providers are disabled. Absolute local artifact directories may contain regular `.whl` files only; wheel bytes and directory membership must remain unchanged during the operation. Cooldown needs timestamped registry artifacts and cannot verify `find-links` inputs.

Verified writes support implicit public PyPI without project index configuration. For custom indexes, use `[[tool.uv.index]]` and named sources; lookup-only `--index` overrides are rejected rather than passed to uv. Verification and Doctor reject index URLs with embedded credentials, even over HTTPS. Use named-index environment credentials as described in [Package indexes and authentication](indexes.md#package-indexes-and-authentication). Normal requirement-only updates can still use `--index`.

Preparation and resolution each have a 120-second limit. Verification does not sync or install the project environment and does not intentionally execute build backends. In particular, `uv lock --no-build` alone is not a safe boundary: uv can execute first-party/editable build backends, so unsupported layouts are rejected first. A temporary directory is not an OS security sandbox.

A verification failure or cancellation before application leaves original inputs unchanged. Application uses guarded replacements of the exact verified requirements and lockfile. If a write fails, recovery restores only files that still contain tool-written bytes; it does not overwrite concurrent user edits. Multi-file application is not transactional, and a concurrent-write window remains. Review the reported files together and restore only intended paths from version control if automatic recovery cannot complete. Child diagnostic output is bounded and suppressed on resolver failure because it can contain credentials. Run uv locally to inspect a resolver error before sharing its diagnostics.

## Doctor updates

```sh
# Put --test-command last; arguments are passed directly, without an implicit shell.
uvx check-updates -u --doctor --test-command uv run --no-sync pytest
```

Doctor is an explicit Unix-only mode. Other platforms fail before starting child processes. It requires an existing lockfile, the supported static verification layout, and a nonempty user-supplied test command. It copies source/tests into a temporary project, excludes `.git`, `.venv`, `target`, `dist`, and cache directories, and refuses symlinks, special files, more than 10,000 files, or more than 64 MiB of inputs. The user's project environment is not synced.

Doctor first resolves, installs, and tests the baseline. It syncs all extras, dependency groups, and workspace packages in the temporary environment. Mutually incompatible optional scopes can therefore fail baseline validation. It checks that selected distributions are installed on the test interpreter; inactive marker/platform branches are not described as tested. Each trial must install the exact selected versions. Doctor then tests the complete candidate batch, tries bounded subsets on failure, and freshly resolves and re-tests the final combined subset before applying it. Individually passing updates are never combined without a final combined test. Updates that only pass together can be observed, but the search is not exhaustive and does not identify a unique failure cause.

Doctor reads distribution metadata in batches of at most 16 KiB. It rejects a serialized name over 4096 bytes or a serialized version over 1024 bytes before application. Diagnostic capture remains limited to 8192 bytes per stream.

The limit is 32 attempts, including baseline and final confirmation, and ten minutes total. Timeouts, interrupts, baseline failures, changed snapshots, altered temporary resolver inputs, or a failed final test prevent application. Raw child output is bounded and suppressed because it can contain secrets. Reports name observed update rows and do not claim that untested combinations passed. Unix child process groups are stopped after commands, including successful commands.

Doctor can use the network and executes dependency and project code with the user's permissions. The temporary project and virtual environment protect normal project state; they do not isolate the host. Only run trusted projects, dependencies, uv executables, and test commands. An observed test pass is not a compatibility or security guarantee.

## Related documentation

[Usage](usage.md) · [Update policies](policies.md) · [Package indexes](indexes.md)
