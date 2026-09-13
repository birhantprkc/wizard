#!/usr/bin/env python3
"""Compare two call logs from review-mock-server.py: review off, then review on.

    python3 bench/review-report.py <label> <off.jsonl> <on.jsonl>

One line: model calls and prompt characters with and without the review, and
what share of each the review itself was.
"""

import json
import sys


def rows(path):
    with open(path) as log:
        return [json.loads(line) for line in log]


def chars(rows):
    return sum(row["prompt_chars"] for row in rows)


def pct(new, old):
    return (new - old) / old * 100 if old else float("nan")


def main():
    label, off_path, on_path = sys.argv[1:4]
    off, on = rows(off_path), rows(on_path)
    review = [row for row in on if row["role"] == "review"]
    calls = f"{len(off)} -> {len(on)} (+{pct(len(on), len(off)):.0f}%)"
    size = f"{chars(off)} -> {chars(on)} (+{pct(chars(on), chars(off)):.0f}%)"
    share = f"{len(review)} calls, {chars(review)} chars"
    print(f"  {label:<7} {calls:<24} {size:<28} {share}")


if __name__ == "__main__":
    main()
