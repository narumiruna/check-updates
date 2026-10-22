[default]
all: format lint test

format:
    cargo fmt --check

lint:
    cargo clippy --all-targets --locked -- -D warnings

test:
    cargo test --locked

build:
    cargo build --release --locked

# Build a local platform wheel and source distribution.
package:
    uv build

# Publish previously built distributions.
publish:
    uv publish
