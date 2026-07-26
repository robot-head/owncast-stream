#!/bin/sh
set -eu

project_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
cargo test --manifest-path "$project_dir/Cargo.toml"
cmp --silent "$project_dir/target/release/owncast-stream" /usr/local/bin/owncast-stream

if ldd "$project_dir/target/release/owncast-stream" | grep -q 'not found'; then
    echo "release binary has missing shared libraries" >&2
    exit 1
fi
