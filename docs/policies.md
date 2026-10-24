# Update policies

[README](../README.md)

## Update policies and decision reports

```sh
# Explain newer versions blocked by bounds, Python metadata, or release policy.
uvx check-updates --explain
uvx check-updates --explain --show-locked --format json --check

# Ignore artifacts uploaded within the last seven days.
uvx check-updates --cooldown 7d --explain

# Avoid raising a minimum requirement that already admits the selected version.
uvx check-updates --minimal --diff

# Check an RC explicitly; development releases remain excluded.
uvx check-updates --pre --include example-package

# Check an exact newer release without writing.
uvx check-updates --to 'example-package==2.4.1' --diff

# Select workspace projects and package-name patterns.
uvx check-updates -w --member api --include-glob 'pytest-*'
```

`--explain` and `--show-locked` are read-only. They cannot be combined with `--diff`, `-u`, or `-i`. They do not run uv. `--check` returns `3` only when the selected scope has requirement edits, not when a newer version is blocked. `--explain` can report empty, prerelease-only, and yanked-only candidate sets without the normal stable-release error. Lookup and malformed-response errors still fail without partial JSON.

The decision report separates the highest observed stable, non-yanked version (`latest`), selected candidate (`selected`), and highest observed non-yanked alpha/beta/RC (`prerelease`). Versions are compared using PEP 440. With `--pre`, the highest eligible version wins: `3rc1` can be preferred over `2`, while `3` is preferred over `3rc1`. Neither `--pre` nor `--to` admits development or yanked releases. Explicit targets must be newer than the declared baseline, pass active target/age/Python/bound rules, and match a supported selected dependency. A target does not authorize a write or an implicit downgrade.

`--minimal` suppresses an edit when the highest eligible candidate already satisfies the full original requirement, including exclusions and the implicit `~=` upper bound. Thus `foo>=1` can stay unchanged; `foo==1` can change. `foo~=1.0` stays unchanged for candidate `1.9`, but can change to `foo~=2.0` for candidate `2.0`, which is outside the old range. Suppressed edits do not cause automatic locking or exit code `3`. This mode is not a lockfile-only upgrade; use uv for that operation.

### Locked version context

`--show-locked` reads `uv.lock` from the selected project/workspace root. It reports all matching registry versions and their package-specific resolution markers, rather than picking one version. Supported lockfiles use version `1`, revisions `0` through `5`; absent revision means `0`. Symlinks, special files, absent records, and unknown sources are not treated as confirmed matches. Missing, malformed, unsupported, and source-mismatch states are explicit. Registry URLs are not printed.

Every report has `freshness: "not_checked"`. Matching metadata does not establish that a lockfile is current, that a version is installed, or that it applies to the running interpreter. Workspace member selection does not change the lockfile root.

### Release cooldown

Cooldown is opt-in and accepts positive integer durations with `d`, `h`, or `m`. One whole-second UTC cutoff is used for the command. Fractional upload times round up to the next second so a newer artifact cannot pass the cutoff. Upload time, Python eligibility, and non-yanked status must pass on the same distribution artifact; the exact cutoff is inclusive. A newer version with only recent artifacts is skipped in favor of an older eligible version. Unknown or malformed timestamps are ineligible while cooldown is active. HTML indexes often have no upload timestamps. No private package metadata is fetched from public PyPI to fill a missing timestamp.

With an existing lockfile, writes using cooldown or prereleases use the [verified path](locking.md#verified-updates). The same effective policies apply to resolution. Cooldown starts a fresh temporary resolution so recent locked artifacts cannot silently remain eligible; this can change transitive versions. Existing uv age/prerelease settings that cannot be combined safely are rejected before writing. Per-package cooldown overrides the check-updates global cooldown. Configured package cooldowns also protect transitive and excluded dependencies during verified locking; package filters limit requirement edits, not configured resolver safety rules. Different effective global age or prerelease policies across editable workspace projects are rejected for lock-writing modes; use one workspace policy. With `--no-lock`, or no existing lockfile, only requirement selection is filtered; no lockfile or transitive-policy guarantee is made. Cooldown reduces immediate exposure to new releases; it does not prove package safety or wheel availability.

### Saved project policy

```toml
[tool.check-updates]
target = "minor"
cooldown = "7d"
exclude = ["legacy-package"]
minimal = false
pre = false

[tool.check-updates.packages.framework]
target = "patch"
cooldown = "14d"
```

Supported keys are `target`, `cooldown`, `include`, `exclude`, `include-glob`, `exclude-glob`, `group`, `extra`, `runtime`, `minimal`, `pre`, and `packages`. Lists contain strings; flags are booleans. Package tables support only `target` and `cooldown`. Unknown keys, invalid types, invalid globs, and duplicate normalized package names are errors. No executable policy files or global check-updates configuration are read.

In `-w`, members inherit root values and override them by field. Lists replace inherited lists, including an explicit empty list. Package tables merge by normalized package name and field. Outside `-w`, only the current project's check-updates settings are read. Explicit CLI scalar values override package and project settings; CLI lists replace corresponding configured lists. Use `--no-pre` and `--no-minimal` to override configured true values. Without section selectors, all supported sections remain selected. A configured policy never enables writing, verification, installation, or tests by itself.

`--member` uses normalized `[project].name`, not a directory name, and requires `-w`. Unknown or ambiguous names are errors. A named root can be selected like a member; an unnamed root cannot be named, but remains part of the resolver/source context. Omit `--member` to select all projects. Existing package filters remain exact; glob inclusion joins exact inclusion, and all exclusion filters win before metadata requests.

## Related documentation

[Usage](usage.md) · [Verified updates](locking.md#verified-updates) · [JSON output](reference.md#automation-and-previews)
