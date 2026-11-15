#!/bin/sh
# Acceptance check: check-release-bump.sh rejects a Cargo-compatible version bump that carries
# breaking evidence, and accepts every other bump, on synthetic repositories.
set -eu

script="$(cd "$(dirname "$0")" && pwd)/check-release-bump.sh"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

if [ ! -f "$script" ]; then
    echo "error: $script does not exist" >&2
    exit 1
fi

failures=0
repo=""

g() {
    git -c user.name=test -c user.email=test@example.invalid \
        -c commit.gpgsign=false -c tag.gpgsign=false "$@"
}

new_repo() {
    repo="$tmp/$1"
    mkdir -p "$repo"
    cd "$repo"
    g init -q
    : >CHANGELOG.md
}

commit() {
    echo "$1" >>file.txt
    g add file.txt
    if [ "$#" -gt 1 ]; then
        g commit -q -m "$1" -m "$2"
    else
        g commit -q -m "$1"
    fi
}

# release VERSION [breaking]: a changelog section for VERSION, committed and tagged vVERSION.
release() {
    {
        echo "# Changelog"
        echo
        echo "## [Unreleased]"
        echo
        echo "## [$1]"
        echo
        echo "### Changed"
        echo
        echo "- an ordinary change (#1)"
        if [ "${2:-}" = "breaking" ]; then
            echo "- **Breaking:** an incompatible change (#2)"
        fi
    } >CHANGELOG.md
    g add CHANGELOG.md
    g commit -q -m "chore: release $1"
    g tag "v$1"
}

# base VERSION: a commit tagged vVERSION with its own plain changelog section.
base() {
    commit "chore: base $1"
    release "$1"
}

run_script() {
    (cd "$repo" && sh "$script" "$1" CHANGELOG.md "v$1")
}

expect_pass() {
    name="$1"
    version="$2"
    head_before=$(cd "$repo" && g rev-parse HEAD)
    if ! out=$(run_script "$version" 2>&1); then
        echo "error: $name was rejected: $out" >&2
        failures=$((failures + 1))
    fi
    check_read_only "$name" "$head_before"
}

expect_fail() {
    name="$1"
    version="$2"
    head_before=$(cd "$repo" && g rev-parse HEAD)
    status=0
    out=$(run_script "$version" 2>&1) || status=$?
    if [ "$status" -ne 1 ]; then
        echo "error: $name exited $status, expected 1: $out" >&2
        failures=$((failures + 1))
    elif ! printf '%s\n' "$out" | grep -q '^error:'; then
        echo "error: $name printed no 'error:' line: $out" >&2
        failures=$((failures + 1))
    fi
    check_read_only "$name" "$head_before"
}

expect_status() {
    name="$1"
    version="$2"
    want="$3"
    head_before=$(cd "$repo" && g rev-parse HEAD)
    status=0
    out=$(run_script "$version" 2>&1) || status=$?
    if [ "$status" -ne "$want" ]; then
        echo "error: $name exited $status, expected $want: $out" >&2
        failures=$((failures + 1))
    elif ! printf '%s\n' "$out" | grep -q '^error:'; then
        echo "error: $name printed no 'error:' line: $out" >&2
        failures=$((failures + 1))
    fi
    check_read_only "$name" "$head_before"
}

check_read_only() {
    if [ -n "$(cd "$repo" && g status --porcelain)" ] ||
        [ "$(cd "$repo" && g rev-parse HEAD)" != "$2" ]; then
        echo "error: $1 modified the repository" >&2
        failures=$((failures + 1))
    fi
}

# Failing cases.

new_repo patch-changelog-breaking
base 0.9.1
commit "fix: a"
release 0.9.2 breaking
expect_fail patch-changelog-breaking 0.9.2

new_repo patch-bang-subject
base 0.9.1
commit "fix!: a"
release 0.9.2
expect_fail patch-bang-subject 0.9.2

new_repo patch-scoped-bang-subject
base 0.9.1
commit "feat(core)!: a"
release 0.9.2
expect_fail patch-scoped-bang-subject 0.9.2

new_repo patch-footer
base 0.9.1
commit "fix: a" "BREAKING CHANGE: a is gone"
release 0.9.2
expect_fail patch-footer 0.9.2

new_repo patch-hyphen-footer
base 0.9.1
commit "fix: a" "BREAKING-CHANGE: a is gone"
release 0.9.2
expect_fail patch-hyphen-footer 0.9.2

new_repo missing-section
base 0.9.1
commit "fix: a"
commit "chore: release 0.9.2 without a changelog section"
g tag v0.9.2
expect_fail missing-section 0.9.2

new_repo major-one-patch
base 1.2.0
commit "feat!: a"
release 1.2.1
expect_fail major-one-patch 1.2.1

new_repo major-one-minor
base 1.2.0
commit "feat!: a"
release 1.3.0 breaking
expect_fail major-one-minor 1.3.0

new_repo breaking-after-prerelease
base 0.9.1
commit "fix: a"
release 0.9.2-rc.1
commit "fix!: b"
release 0.9.2
expect_fail breaking-after-prerelease 0.9.2

new_repo prerelease-with-breaking
base 0.9.1
commit "fix: a"
release 0.9.2-rc.1 breaking
expect_fail prerelease-with-breaking 0.9.2-rc.1

new_repo prerelease-without-own-section
base 0.9.1
commit "fix: a"
release 0.9.2
g tag -d v0.9.2 >/dev/null
g tag v0.9.2-rc.1
expect_fail prerelease-without-own-section 0.9.2-rc.1

# Malformed versions fail closed with exit 2.

new_repo malformed-new-version
base 1.2.0
commit "feat!: a"
release 1x.3.0 breaking
expect_status malformed-new-version 1x.3.0 2

new_repo two-component-version
base 1.2.0
commit "feat!: a"
release 1.2 breaking
expect_status two-component-version 1.2 2

new_repo four-component-version
base 1.2.0
commit "feat!: a"
release 1.2.3.4 breaking
expect_status four-component-version 1.2.3.4 2

new_repo malformed-base-tag
base 1.2.x
commit "feat!: a"
release 1.2.1 breaking
expect_status malformed-base-tag 1.2.1 2

# Passing cases.

new_repo minor-with-breaking
base 0.9.1
commit "feat!: a"
release 0.10.0 breaking
expect_pass minor-with-breaking 0.10.0

new_repo clean-patch
base 0.9.1
commit "fix: a"
release 0.9.2
expect_pass clean-patch 0.9.2

new_repo first-release
commit "feat!: a"
release 0.1.0 breaking
expect_pass first-release 0.1.0

new_repo prerelease-then-minor
base 0.9.1
commit "feat!: a"
release 0.10.0-rc.1 breaking
commit "fix: b"
release 0.10.0 breaking
expect_pass prerelease-then-minor 0.10.0

new_repo major-bump-with-breaking
base 1.2.0
commit "feat!: a"
release 2.0.0 breaking
expect_pass major-bump-with-breaking 2.0.0

new_repo zero-zero-patch-with-breaking
base 0.0.3
commit "feat!: a"
release 0.0.4 breaking
expect_pass zero-zero-patch-with-breaking 0.0.4

new_repo prerelease-with-own-section
base 0.9.1
commit "fix: a"
release 0.9.2-rc.1
expect_pass prerelease-with-own-section 0.9.2-rc.1

if [ "$failures" -ne 0 ]; then
    exit 1
fi

echo "all release bump cases behave"
