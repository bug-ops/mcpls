#!/usr/bin/env python3
"""Fail when a production tracing call interpolates values into its message.

Every `trace!`/`debug!`/`info!`/`warn!`/`error!`/`event!` message must be a
constant literal; variable data belongs in structured fields. Test modules
are exempt. Run from the repository root; sources must be rustfmt-formatted.
"""

import glob
import os
import re
import sys

MACRO = re.compile(r"(?<![A-Za-z0-9_])(?:tracing::)?(trace|debug|info|warn|error|event)!\s*\(")
CFG_ATTR = re.compile(r"^\s*#\[cfg\((.*)\)\]\s*$")
MOD_INLINE = re.compile(r"^(\s*)(?:pub(?:\([^)]*\))?\s+)?mod\s+\w+\s*\{\s*$")
MOD_FILE = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*;\s*$")
PATH_ATTR = re.compile(r'^\s*#\[path\s*=\s*"([^"]+)"\]\s*$')
ATTR_OR_DOC = re.compile(r"^\s*(?:#\[|///)")
RAW_STR = re.compile(r'b?r(#*)"')
CHAR_LIT = re.compile(r"b?'(?:\\[^']*|[^'\\])'")


def split_top_level(expr):
    parts, depth, cur = [], 0, ""
    for c in expr:
        if c == "," and depth == 0:
            parts.append(cur.strip())
            cur = ""
            continue
        depth += (c == "(") - (c == ")")
        cur += c
    return parts + [cur.strip()] if cur.strip() else parts


def is_test_cfg(expr):
    """True when `expr` holds only if `test` is set (`test`, `all(test, ..)`)."""
    expr = expr.strip()
    if expr == "test":
        return True
    if expr.startswith("all(") and expr.endswith(")"):
        return any(is_test_cfg(a) for a in split_top_level(expr[4:-1]))
    return False


def is_test_attr(line):
    m = CFG_ATTR.match(line)
    return bool(m) and is_test_cfg(m.group(1))


def skip_literal(src, i):
    """Return the index just past the literal starting at `i`, or `i` if none."""
    m = RAW_STR.match(src, i)
    if m:
        end = src.find('"' + m.group(1), m.end())
        return len(src) if end < 0 else end + 1 + len(m.group(1))
    if src.startswith('"', i) or src.startswith('b"', i):
        j = i + (2 if src[i] == "b" else 1)
        while j < len(src) and src[j] != '"':
            j += 2 if src[j] == "\\" else 1
        return j + 1
    m = CHAR_LIT.match(src, i)
    return m.end() if m else i


def call_args(src, start):
    """Split the top-level arguments of the call whose `(` precedes `start`."""
    args, depth, cur, i = [], 1, start, start
    while i < len(src) and depth:
        c = src[i]
        if src.startswith("//", i):
            nl = src.find("\n", i)
            i = len(src) if nl < 0 else nl
            continue
        j = skip_literal(src, i)
        if j != i:
            i = j
            continue
        if c in "([{":
            depth += 1
        elif c in ")]}":
            depth -= 1
            if depth == 0:
                args.append(src[cur:i])
        elif c == "," and depth == 1:
            args.append(src[cur:i])
            cur = i + 1
        i += 1
    return [a.strip() for a in args if a.strip()]


def message(args):
    """The first argument that is a bare string literal (not `k = "v"`)."""
    for a in args:
        if re.fullmatch(r'b?r?#*".*"#*', a, re.S):
            return a
    return None


def excluded_inline_lines(lines):
    """Line indexes covered by inline `#[cfg(test)] mod name { .. }` blocks."""
    skipped, i = set(), 0
    while i < len(lines):
        if is_test_attr(lines[i]):
            j = i + 1
            while j < len(lines) and ATTR_OR_DOC.match(lines[j]):
                j += 1
            m = MOD_INLINE.match(lines[j]) if j < len(lines) else None
            if m:
                close = m.group(1) + "}"
                k = j + 1
                while k < len(lines) and lines[k].rstrip() != close:
                    k += 1
                skipped.update(range(i, k + 1))
                i = k
        i += 1
    return skipped


def excluded_modules(path, lines):
    """Out-of-line `#[cfg(test)] mod x;` targets as (file, directory) pairs."""
    out, here = [], os.path.dirname(path)
    stem = os.path.splitext(os.path.basename(path))[0]
    moddir = here if stem in ("lib", "main", "mod") else os.path.join(here, stem)
    for i, line in enumerate(lines):
        if not is_test_attr(line):
            continue
        j, explicit = i + 1, None
        while j < len(lines) and ATTR_OR_DOC.match(lines[j]):
            p = PATH_ATTR.match(lines[j])
            explicit = p.group(1) if p else explicit
            j += 1
        m = MOD_FILE.match(lines[j]) if j < len(lines) else None
        if not m:
            continue
        if explicit:
            target = os.path.normpath(os.path.join(here, explicit))
            out.append((target, os.path.splitext(target)[0]))
        else:
            name = m.group(1)
            out.append((os.path.join(moddir, name + ".rs"), os.path.join(moddir, name)))
            out.append((os.path.join(moddir, name, "mod.rs"), os.path.join(moddir, name)))
    return out


def main():
    files = sorted(glob.glob("crates/*/src/**/*.rs", recursive=True))
    texts = {f: open(f, encoding="utf-8").read() for f in files}
    skip_files, skip_dirs = set(), []
    for f, t in texts.items():
        for target, d in excluded_modules(f, t.split("\n")):
            skip_files.add(os.path.normpath(target))
            skip_dirs.append(os.path.normpath(d) + os.sep)
    scanned, violations = 0, []
    for f, src in texts.items():
        nf = os.path.normpath(f)
        if nf in skip_files or any(nf.startswith(d) for d in skip_dirs):
            continue
        lines = src.split("\n")
        skipped = excluded_inline_lines(lines)
        for m in MACRO.finditer(src):
            line = src.count("\n", 0, m.start())
            if line in skipped or lines[line].lstrip().startswith("//"):
                continue
            scanned += 1
            msg = message(call_args(src, m.end()))
            if msg is not None and "{" in msg.replace("{{", "").replace("}}", ""):
                violations.append(f"{f}:{line + 1}: {m.group(1)}! {msg}")
    if scanned == 0:
        print("no tracing calls scanned; run from the repository root", file=sys.stderr)
        return 2
    for v in violations:
        print(v)
    return 1 if violations else 0


if __name__ == "__main__":
    sys.exit(main())
