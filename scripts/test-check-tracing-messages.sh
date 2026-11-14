#!/bin/sh
# Regression check: check-tracing-messages.py must flag, skip and report the right calls in a synthetic tree.
# Run from the repository root.
set -eu

checker="$PWD/scripts/check-tracing-messages.py"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

failures=0
out=""
rc=0

fail() {
    echo "error: $1" >&2
    failures=$((failures + 1))
}

run_checker() {
    rc=0
    out=$(cd "$tmp" && python3 "$checker" 2>&1) || rc=$?
}

expect_flagged() {
    printf '%s\n' "$out" | grep -qF -- "$1" || fail "not flagged: $1"
}

expect_silent() {
    if printf '%s\n' "$out" | grep -qF -- "$1"; then
        fail "wrongly flagged: $1"
    fi
}

write_src() {
    mkdir -p "$tmp/crates/demo/src"
    cat >"$tmp/crates/demo/src/$1"
}

write_src lib.rs <<'EOF'
mod flagged;
mod ok;
mod reported;

#[cfg(test)]
mod tests_inline {
    fn t() {
        info!("skip_inline {x}");
    }
}

#[cfg(all(test, unix))]
mod tests_all_inline {
    fn t() {
        info!("skip_all_inline {x}");
    }
}

#[cfg(test)]
mod tests_out_of_line;

#[cfg(test)]
#[path = "custom/explicit.rs"]
mod tests_explicit;
EOF

write_src tests_out_of_line.rs <<'EOF'
fn t() {
    info!("skip_out_of_line {x}");
}
EOF

mkdir -p "$tmp/crates/demo/src/custom"
cat >"$tmp/crates/demo/src/custom/explicit.rs" <<'EOF'
fn t() {
    info!("skip_explicit_path {x}");
}
EOF

write_src flagged.rs <<'EOF'
fn f() {
    info!("flag_single {x}");
    error!(
        field = 1,
        "flag_multi {x}"
    );
    tracing::event!(tracing::Level::INFO, "flag_event {x}");
    warn!(r#"flag_raw {x} ) "quoted""#);
    info!(target: "t", "flag_target {x}");
}
EOF

write_src ok.rs <<'EOF'
fn f() {
    info!(k = "skip_field {y}", "constant");
    info!("skip_escaped {{x}}");
    info!(r"skip_raw", k = r#"{y}"#);
    // info!("skip_comment {x}");
}
EOF

write_src reported.rs <<'EOF'
#[cfg(all(not(test), unix))]
mod not_test {
    fn f() {
        info!("report_not_test {x}");
    }
}

#[cfg(feature = "test-utils")]
mod test_utils {
    fn f() {
        info!("report_test_utils {x}");
    }
}
EOF

run_checker
[ "$rc" -eq 1 ] || fail "synthetic tree exited $rc, expected 1"

for msg in flag_single flag_multi flag_event flag_raw flag_target report_not_test report_test_utils; do
    expect_flagged "$msg {x}"
done

for msg in skip_inline skip_all_inline skip_out_of_line skip_explicit_path skip_field skip_escaped skip_raw skip_comment; do
    expect_silent "$msg"
done

rm -rf "$tmp/crates"
write_src lib.rs <<'EOF'
fn f() {
    info!("constant");
}
EOF
run_checker
[ "$rc" -eq 0 ] || fail "clean tree exited $rc, expected 0"

rm -rf "$tmp/crates"
write_src lib.rs <<'EOF'
fn f() {}
EOF
run_checker
[ "$rc" -eq 2 ] || fail "tree without tracing calls exited $rc, expected 2"

if [ "$failures" -ne 0 ]; then
    exit 1
fi

echo "all tracing message checker cases behave as expected"
