#!/bin/bash

# Exit immediately if a command exits with a non-zero status.
set -e

echo "Running fmt check..."
cargo fmt --all -- --check

echo "Running clippy check..."
cargo clippy --all-targets --all-features -- -D warnings

echo "Building..."
cargo build --verbose

echo "Running tests with nextest..."
cargo nextest run 

echo "All checks passed! 🎉"
