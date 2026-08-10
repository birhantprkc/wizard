#!/usr/bin/env python3
"""List public functions in `src/` that nothing outside a test calls.

    contrib/find-unwired.py

An audit aid, not a gate. It always exits 0 and it is not wired into CI: most
of what it prints is legitimate (a constructor for external callers, a test
seam whose doc says so, the owned twin of a borrowed accessor). Read the list;
do not act on it mechanically.

WHY THIS EXISTS

This tree has a recurring defect its progress ledger calls "a seam built
correctly on both sides and never joined in the middle": a function that is
written, documented and unit-tested, but that nothing in a real run calls, so
the behaviour its doc comment describes silently does not happen. Four have
been found so far and every one of them was a real bug a user could see:

  - `usage::record_cache` — every turn billed as all-fresh input.
  - `UsageTracker::session_cache_totals` — `/cost` billed every cached token at
    the full input rate and disagreed with `wizard usage` by close to tenfold.
  - `Agent::task_registry` — `/bashes` answered "unavailable while a turn is
    running", during a turn, which is the only time it has anything to list.
  - a stream-retry handler that left the failed attempt on screen.

None of them could fail a test. The module reads as finished, the tests are
green, and the doc comment describes behaviour the wiring does not deliver.
There is no disabled constant to grep for and no dead-code warning, because
`pub` suppresses it. The tell is the absence of a call.

WHAT IT DOES

Finds every `pub`/`pub(crate)`/`pub(super)` fn declared outside a test module,
then reports the ones whose name appears nowhere else in non-test code.
`#[cfg(test)] mod tests {` truncates a file; `#[cfg(test)] mod tests;` (the
external-file form) skips two lines and keeps going. `tests.rs` files and
anything under a `tests/` directory count as test code throughout.

It matches on the bare name, so it over-reports nothing and under-reports a
little: a function reachable only through a trait object or a macro looks
called when it is not. Confirm anything it flags before believing it.
"""

import collections
import pathlib
import re
import subprocess
import sys

DECL = re.compile(r"^\s*(?:pub|pub\(crate\)|pub\(super\))\s+(?:async\s+)?fn\s+([a-z_][a-z0-9_]*)")
WORD = re.compile(r"\b[a-z_][a-z0-9_]*\b")


def spans(path, lines):
    """(cut, skipped) — where test code starts, and lines to ignore before it."""
    if path.name == "tests.rs" or "tests" in path.parts:
        return 0, set()
    skipped = set()
    for i, line in enumerate(lines):
        if line.strip() != "#[cfg(test)]":
            continue
        following = lines[i + 1].strip() if i + 1 < len(lines) else ""
        if following.endswith("{"):
            return i, skipped
        # `#[cfg(test)] mod tests;` and friends: two lines, then carry on.
        skipped.update((i, i + 1))
    return len(lines), skipped


def main():
    root = pathlib.Path(
        subprocess.run(
            ["git", "rev-parse", "--show-toplevel"],
            capture_output=True, text=True, check=True,
        ).stdout.strip()
    ) / "src"

    files = sorted(root.rglob("*.rs"))
    lines = {f: f.read_text().splitlines() for f in files}
    bounds = {f: spans(f, lines[f]) for f in files}

    declared, mentions = [], collections.Counter()
    for f in files:
        cut, skipped = bounds[f]
        for i, line in enumerate(lines[f]):
            if i >= cut or i in skipped:
                continue
            found = DECL.match(line)
            if found:
                declared.append((found.group(1), f, i + 1))
            mentions.update(WORD.findall(line))

    # A declaration mentions its own name once, in its signature.
    unwired = [(n, f, l) for n, f, l in declared if mentions[n] <= 1]

    for name, f, line in unwired:
        print(f"{f.relative_to(root.parent)}:{line}\t{name}")
    print(
        f"\n{len(declared)} public fns, {len(unwired)} with no non-test caller.",
        file=sys.stderr,
    )
    print(
        "Triage each: a seam defect (wire it), dead (delete it), or deliberate "
        "(say so in its doc comment, so the next sweep can skip it).",
        file=sys.stderr,
    )


if __name__ == "__main__":
    main()
