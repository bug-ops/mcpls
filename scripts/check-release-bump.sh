#!/bin/sh
# Fail a release whose version is Cargo-compatible with the previous release but carries breaking evidence.
# Usage: check-release-bump.sh <version> [changelog=CHANGELOG.md] [ref=v<version>]
set -eu

if [ "$#" -lt 1 ] || [ "$#" -gt 3 ]; then
    echo "usage: $0 <version> [changelog] [ref]" >&2
    exit 2
fi

version="$1"
changelog="${2:-CHANGELOG.md}"
ref="${3:-v$version}"

bad_version() {
    echo "error: '$1' is not a semantic version" >&2
    exit 2
}

core_of() {
    core="${1%%+*}"
    core="${core%%-*}"
    case "$core" in
        *[!0-9.]* | "" | .* | *. | *..*) bad_version "$1" ;;
    esac
    case "$core" in
        *.*.*.*) bad_version "$1" ;;
        *.*.*) ;;
        *) bad_version "$1" ;;
    esac
    printf '%s\n' "$core"
}

new_core=$(core_of "$version")
errors=""

add_error() {
    errors="$errors
error: $1"
}

section=$(awk -v header="## [$version]" '
    index($0, "## [") == 1 { on = (index($0, header) == 1); next }
    on
' "$changelog" 2>/dev/null || true)
if ! grep -qF "## [$version]" "$changelog" 2>/dev/null; then
    add_error "$changelog has no section '## [$version]'"
fi

# Prerelease tags are never a base, so a release is compared with the previous stable one.
base=$(git describe --tags --abbrev=0 --match 'v[0-9]*' --exclude 'v*-*' "$ref^" 2>/dev/null || true)

# A change is Cargo-compatible when the leftmost non-zero component is unchanged; 0.0.x is never compatible.
compatible=0
if [ -n "$base" ]; then
    base_core=$(core_of "${base#v}")
    IFS=. read -r base_major base_minor _ <<EOF
$base_core
EOF
    IFS=. read -r new_major new_minor _ <<EOF
$new_core
EOF
    if [ "$base_major" -gt 0 ]; then
        [ "$new_major" -eq "$base_major" ] && compatible=1
    elif [ "$base_minor" -gt 0 ]; then
        [ "$new_major" -eq 0 ] && [ "$new_minor" -eq "$base_minor" ] && compatible=1
    fi
fi

if [ "$compatible" -eq 1 ]; then
    evidence=""
    changelog_hits=$(printf '%s\n' "$section" | grep -F '**Breaking:**' || true)
    if [ -n "$changelog_hits" ]; then
        evidence="$evidence
$(printf '%s\n' "$changelog_hits" | sed "s|^|  $changelog: |")"
    fi
    for sha in $(git rev-list "$base..$ref"); do
        subject=$(git log -1 --format=%s "$sha")
        if printf '%s\n' "$subject" | grep -Eq '^[a-z]+(\([^)]*\))?!:'; then
            evidence="$evidence
  $sha $subject"
        elif git log -1 --format=%b "$sha" | grep -Eq '^BREAKING[ -]CHANGE:'; then
            evidence="$evidence
  $sha $subject (BREAKING CHANGE footer)"
        fi
    done
    if [ -n "$evidence" ]; then
        add_error "$version is semver-compatible with $base but has breaking changes; bump the leftmost non-zero component:$evidence"
    fi
fi

if [ -n "$errors" ]; then
    printf '%s\n' "${errors#
}" >&2
    exit 1
fi

echo "release bump $version is consistent (base: ${base:-none})"
