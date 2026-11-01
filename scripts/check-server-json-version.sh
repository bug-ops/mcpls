#!/bin/sh
# Verify that server.json matches the workspace version and carries no trust-relaxing inputs.
set -eu

manifest="${1:-server.json}"

workspace_version=$(cargo metadata --no-deps --format-version 1 |
    jq -r '.packages[] | select(.name == "mcpls") | .version')

if [ -z "$workspace_version" ]; then
    echo "error: package mcpls not found in cargo metadata" >&2
    exit 1
fi

errors=$(jq -r --arg v "$workspace_version" '
    [
      (if .version != $v then "version \(.version) != workspace version \($v)" else empty end),
      (if (.packages | length) == 0 then "no packages declared" else empty end),
      (.packages[]? |
        (if .version != $v then "package \(.identifier) version \(.version) != workspace version \($v)" else empty end),
        (if .registryType != "cargo" then "package \(.identifier) has registryType \(.registryType), expected cargo" else empty end),
        (if .transport.type != "stdio" then "package \(.identifier) transport is not stdio" else empty end),
        (if has("environmentVariables") or has("runtimeArguments") or has("packageArguments")
         then "package \(.identifier) declares environment variables or arguments" else empty end)),
      (if has("environmentVariables") or has("runtimeArguments") or has("packageArguments")
       then "top-level environment variables or arguments declared" else empty end)
    ] | .[]
' "$manifest")

if [ -n "$errors" ]; then
    printf '%s\n' "$errors" | sed "s|^|error: $manifest: |" >&2
    exit 1
fi

echo "$manifest matches workspace version $workspace_version"
