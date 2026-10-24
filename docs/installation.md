# Installation

[README](../README.md)

The Cargo package name is `py-check-updates`. The PyPI package name and installed command remain `check-updates`.

The command remains available through PyPI. Maturin packages the Rust executable as a platform wheel; there are no Python application dependencies.

```sh
# Run without installing into the project.
uvx check-updates

# Install a persistent command.
uv tool install check-updates

# Or use pip.
pip install check-updates

# Install directly with Cargo, without Python.
cargo install --path . --locked
```

Release builds produce wheels for Linux (glibc 2.28 or newer, x86-64 and ARM64), macOS (Intel and Apple Silicon), and Windows (x86-64). Wheels do not require Rust. Installation from a source distribution requires a Rust toolchain and a C compiler. PyPI installation requires Python 3.10 or newer; the installed executable does not use Python at runtime.

The Docker image contains the native CLI and uv for automatic locking. It does not require Python to run the CLI.

## Related documentation

[Development](development.md) · [Automatic locking](locking.md#automatic-locking)
