# Package indexes and caching

[README](../README.md)

## Package indexes and authentication

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

With `-w`, root definitions are inherited and member definitions override matching names. Non-explicit indexes are tried in declaration order, with `default = true` indexes last. A default index replaces the implicit PyPI fallback, even when it is also explicit. Explicit indexes are queried only for packages assigned to them. If all eligible indexes return 404, or no index is eligible, a configured default still blocks PyPI. A named source uses only that index and never falls back. Undefined names and marker-specific index assignments are errors; marker-dependent routing is not evaluated. Update lookups do not read global uv configuration, pip configuration, keyrings, or index-strategy settings.

Index sources with `extra = "cpu"` apply only to occurrences in `[project.optional-dependencies].cpu`. Each checked occurrence uses its own extra, including when the same package appears in several extras. Extra names use normalized matching. Runtime dependencies and dependency groups do not use extra-qualified sources. If no qualified source matches, normal index selection applies. An unqualified source still applies to every occurrence. Multiple matching index names for one occurrence are an error; marker-specific routing remains unsupported.

For named indexes, supply HTTP Basic credentials using `UV_INDEX_INTERNAL_USERNAME` and `UV_INDEX_INTERNAL_PASSWORD` (uppercase the index name and replace every non-alphanumeric character with an underscore). Alternatively, credentials can be URL-encoded in the URL userinfo; environment credentials override URL credentials. Prefer the named-index environment method so secrets do not appear in shell history or process arguments. The `authenticate` setting accepts `auto` (the default), `always`, or `never`. `auto` uses URL or named environment credentials when present. `always` requires these credentials before a request. `never` rejects URL or named environment credentials before a request and otherwise sends no `Authorization` header. Unknown values are errors. These rules also apply to named sources. No credential providers are used.

Authenticated requests require HTTPS. Explicit ports must be between `1` and `65535`; port `0` is rejected before a request. URLs with queries or fragments are rejected. Cross-origin redirects and HTTPS downgrades are rejected; same-origin redirects retain authentication.

Index errors and diagnostic messages do not expose credentials or private response bodies. Output links omit URL userinfo. Paths and package names can still be sensitive. Private-index metadata is kept only in memory for the invocation and is never written to disk.

A public PyPI 404 stops the check with a message naming the package and suggesting checks for misspellings, private or unpublished packages, and workspace configuration. No files are written when a lookup fails.

## Cache and retries

Public PyPI release metadata is cached for one hour under `$XDG_CACHE_HOME/check-updates`, or `~/.cache/check-updates` when that variable is unset, empty, or not an absolute path. The versioned cache stores artifact versions, Python constraints, upload times, yanked status, and a safe public release-note link; it does not store credentials or response bodies. Corrupt or expired entries are fetched again. Cache-write failures do not fail the check. Use `--no-cache` for fresh lookups without persistent reads or writes. Per-invocation metadata sharing still applies.

Metadata requests use a 15-second timeout and up to three attempts. Ctrl+C cancels an active request, response read, or retry delay and returns exit code `130`. No project files are written during metadata lookup. Network connection failures, truncated responses, HTTP protocol errors, and HTTP `408`, `429`, `500`, `502`, `503`, and `504` are retried after 0.25 and 0.5 seconds. Failed response reads return a normal error after the last attempt. Error output does not include response data. Other HTTP errors are not retried. No dependency files are written until all lookups succeed.

All requirement-file reads and index lookups finish before writing, so lookup failures do not cause partial updates. Default multi-file writes are sequential, not transactional: if a write fails, earlier successful updates remain. Verified application instead attempts guarded recovery as described in [Verified updates](locking.md#verified-updates). Use the output and version control to review or restore changes.

## Related documentation

[Usage](usage.md) · [Verified updates](locking.md#verified-updates)
