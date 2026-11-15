#!/usr/bin/env bash
set -e
taskkill //F //IM okra-brown-search.exe 2>/dev/null || true
cargo build --release
start "" target/release/okra-brown-search.exe
