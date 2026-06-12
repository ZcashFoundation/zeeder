#!/usr/bin/env bash

set -euo pipefail

echo "Checking formatting..."
cargo fmt --all -- --check

echo "Checking diff whitespace..."
git diff --check

echo "Building..."
cargo build

echo "Running clippy..."
cargo clippy --all-targets --all-features -- -D warnings

echo "Running cargo tests..."
cargo test

echo "Running tests with nextest..."
cargo nextest run

echo "All checks passed."
