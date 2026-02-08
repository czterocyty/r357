#!/usr/bin/env bash
set -eux

cargo build --release
cargo build --release --target aarch64-unknown-linux-gnu
cargo build --release --target arm-unknown-linux-gnueabihf
cargo build --release --target arm-unknown-linux-gnueabi
