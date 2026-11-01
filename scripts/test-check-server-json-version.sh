#!/bin/sh
# Regression check: check-server-json-version.sh must pass on the real manifest and fail on each broken variant.
set -eu

script="scripts/check-server-json-version.sh"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

"$script" server.json >/dev/null

failures=0

expect_failure() {
    name="$1"
    filter="$2"
    jq "$filter" server.json >"$tmp/$name.json"
    if "$script" "$tmp/$name.json" >/dev/null 2>&1; then
        echo "error: $name was accepted" >&2
        failures=$((failures + 1))
    fi
}

expect_failure wrong-version '.version = "0.0.0-mismatch"'
expect_failure wrong-package-version '.packages[0].version = "0.0.0-mismatch"'
expect_failure runtime-arguments '.packages[0].runtimeArguments = []'
expect_failure package-arguments '.packages[0].packageArguments = []'
expect_failure environment-variables '.packages[0].environmentVariables = []'
expect_failure non-cargo-package '.packages[0].registryType = "npm"'
expect_failure non-stdio-transport '.packages[0].transport.type = "streamable-http"'
expect_failure no-packages '.packages = []'

if [ "$failures" -ne 0 ]; then
    exit 1
fi

echo "all negative cases rejected"
