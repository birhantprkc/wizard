---
name: coding
description: Baseline coding workflow. Explore before editing, make minimal precise diffs, verify with the project's own build and tests, clean deliverable directories, and review the diff before finishing.
---

# Coding guidelines

## Workflow

1. **Orient first.** Before changing anything, build a picture of the code.
   Use `list_files` to see the layout, `search_files` to find relevant
   symbols, and `read_file` to read the code you are about to touch. Check
   `git_status` for the starting state of the working tree.
2. **Honor project instructions.** If the repo has an `AGENTS.md` or
   `WIZARD.md`, its build/test commands, style rules, and forbidden
   directories override your defaults. The user message is the contract —
   extract required paths, formats, IDs, and cleanup constraints before coding.
3. **Plan briefly, then act.** State what you intend to change and why in a
   sentence or two, then make the change. Do not narrate every tool call.
4. **Make the smallest change that solves the task.** Prefer `edit_file`
   with an exact, unique `old_string` over rewriting whole files with
   `write_file`. Do not reformat, rename, or "clean up" code you were not
   asked to touch.
5. **Verify every change.** Run the project's own commands via `execute`,
   pulled from `AGENTS.md` when present, otherwise inferred from the repo
   (`Cargo.toml` → `cargo check` / `cargo test`, `package.json` →
   `npm test`, `pyproject.toml` → `pytest`, `Makefile` → `make test`).
6. **Leave the deliverable tree clean.** If you compile, build, or generate
   helper binaries next to a required source file, remove those byproducts
   before finishing when the task expects a specific final file set.
7. **Review before declaring done.** Run `git_diff` and read your own
   changes. Confirm they compile, pass tests, and contain nothing
   unintended.

## Spec-driven / report tasks

When the user gives an explicit contract (function signature, CLI flags,
exit codes, JSON/JSONL reports, CWE IDs, polyglot file, exact paths):

1. Extract requirements as a short checklist (or `todo` list for multi-step work).
   Put every required output path on that checklist on turn 1.
2. Implement the general case that covers every requirement — not only examples.
3. For vulnerability / CWE / classification reports, follow this order:
   1. **Locate the defect fast** — run the project's tests early, or
      `search_files` for validation helpers (header setters, input sanitizers,
      `ValueError`, control-char checks). Prefer git history of those helpers
      over broad web CVE research.
   2. **Apply the minimal code fix** so invalid inputs raise the expected
      exception type (often `ValueError`) instead of being ignored.
   3. **Write the report immediately** to the required path with the
      demonstration schema — before long narration or extra research.
   4. **IDs must come from the task's listed CWE vocabulary only** (lowercased
      `cwe-N` as in the demonstration). For CRLF / HTTP header injection /
      response-splitting style bugs when the task lists them, report at least
      `cwe-93` (and `cwe-20` when also listed for improper validation).
      **Never** write only an unlisted synonym such as `cwe-113` / `CWE-113`.
   5. Match schema literally:
      - `cwe_id` is a **list of strings** like `["cwe-93", "cwe-20"]`, not a
        bare string and not `"CWE-113"`.
      - Prefer the demonstration path style (often absolute `/app/...`).
   6. Re-run `pytest` (or the stated command) and self-check:
      ```bash
      python3 -c 'import json; o=json.loads(open("<report-path>").readline());
      assert isinstance(o["cwe_id"], list);
      ids=[x.lower() for x in o["cwe_id"]];
      # assert each required task-listed ID is present
      print(o)'
      ```
4. Self-test with `execute` against those requirements before finishing.
5. If a hard constraint forbids touching a file, never edit it.
6. Before finishing, confirm every required deliverable path exists (`ls`/`cat`).

## Polyglots, dual-language, and build-verify loops

- You may compile and run binaries to verify correctness.
- Write compiled outputs (`a.out`, `cmain`, `*.o`, `*.so`, `__pycache__`,
  `.venv`, etc.) under `/tmp` when possible, **or delete them from the
  deliverable directory before finishing**.
- Final directory checks often require *only* the named source/output file(s).
  After verification: `rm -f` build products left beside those files.
- Example: for `/app/polyglot/main.py.c`, compile to `/tmp/cmain` (or remove
  `/app/polyglot/cmain` before exit) so the polyglot dir contains only
  `main.py.c`.

## Long-lived services (HTTP, QEMU, daemons)

- Verifiers often run **after** the agent session ends. Processes must survive
  without the agent.
- Start them with OS detachment, not the tool's managed background mode:
  ```bash
  nohup python3 -m http.server 8080 --directory /var/www/html \
    > /var/log/webserver.log 2>&1 &
  # or: nohup qemu-system-x86_64 ... > /tmp/qemu.log 2>&1 &
  ```
- Optionally install a restart helper (`/usr/local/bin/start-*.sh` or a small
  `/etc/init.d/...` script) that uses the same `nohup` pattern.
- **Do not** rely on `execute(..., run_in_background=true)` for anything the
  external test harness must still talk to after you finish — those tasks are
  agent-scoped and get cleaned up.
- After starting, prove liveness: `curl`/`ss`/`pgrep`/`nc` against the required
  port. Re-check once more immediately before declaring done.
- Keep the service setup simple once e2e works; avoid thrashing hooks and
  ownership until the primary curl/clone/push path is green.

## Install packages with native/Cython extensions

When the task requires a package installed into the **system** Python with
compiled extensions, follow this **timeboxed critical path** (timeouts are
fails — long inventory greps after a green install are the main failure mode):

1. **Clone + minimal fix batch first.** Apply the small set of known runtime
   breaks that block core import (e.g. Numpy 2 aliases `np.int`→`np.int_`,
   `np.float`→`np.float64`, `np.bool`→`np.bool_`; `from fractions import gcd`
   → `from math import gcd`; soft-import optional `vispy`/visualise; cast
   float divisions used as sizes with `int(...)`). Prefer one scripted
   multi-file replace over many read/edit turns.
2. **System-install immediately after the first fix batch** — do not spend the
   first half of the budget grepping optional modules:
   ```bash
   python3 setup.py build_ext --inplace
   python3 setup.py install   # or pip install . --no-build-isolation after a real .so build
   ```
   Do not stop at in-tree `.so` files alone. `build_ext --inplace` is not a
   finished install.
3. Verify from a **clean path** so local source cannot hide a bad install:
   ```bash
   cd /tmp && python3 -c "import pkg; from pkg.mod import ext; print(ext.__file__)"
   ```
   Confirm extension modules resolve under `site-packages` (`.so`), not only
   under the source checkout. Confirm pure-Python modules (e.g. `make.torus`)
   load from site-packages without stale `fractions.gcd` errors. Run the
   task's required README/snippet here.
4. If `pip install .` produces a pure-Python wheel (no `.so` in site-packages),
   switch to `setup.py build_ext --inplace && setup.py install` (or copy the
   built extensions into the installed tree) and re-verify from `/tmp`.
5. **Immediately run the allowed package tests** (exclude only files the task
   marks broken). Do this in the **same phase** as the green snippet — not
   after another full-repo alias inventory. Example:
   ```bash
   cd /path/to/checkout && python3 -m pytest tests -q \
     --ignore=tests/test_random_curves.py --ignore=tests/test_catalogue.py
   ```
6. **Fix the first real suite failure, then reinstall.** Common post-install
   failures are not more `np.int` hits — they are third-party API drift:
   - Graph/layout packages renaming node/edge attributes
     (`KeyError: 'pos'` / `'start'` / `'end'`): use dual-key access
     `data.get('pos', data.get('vertex_position'))` (and the same for
     `start`/`vertex_start`, `end`/`vertex_end`, edge `pos`/`edge_position`).
   - Size/index errors from float divisions → wrap with `int(...)`.
   - Syntax-broken unused stubs → make them import-safe if imported.
   After the fix: `python3 setup.py install` (or `--force`) again, then
   re-run the **same** allowed suite from the checkout using the system
   install. Do not claim done on a partial file subset if the task requires
   the full allowed suite.
7. **After every later source fix, reinstall** so site-packages is not left
   stale while only the checkout is fixed. Then re-check from `/tmp` **and**
   re-run allowed tests.
8. Soft-import truly optional visualization deps only if hard imports block the
   required APIs. After the required snippet + **full allowed test suite**
   pass, **stop** — do not open another broad grep/inventory loop. Cap: at most
   one targeted remaining-alias grep after the first green clean-path snippet;
   prefer pytest over inventory.

## Image / board / puzzle analysis

- Do **not** dump full pixel grids, pairwise IoU matrices, or huge feature
  tables into the chat — those blow context and can crash the session.
- Keep analysis in **one or two compact scripts** that print only summaries.
- For chess (and similar mate puzzles), preferred pipeline:
  1. **Install early** in one command: `python-chess`, `pillow`, `numpy`, and
     `stockfish` (`apt-get` / `pip`, with `--break-system-packages` if needed).
  2. **Occupancy + color first** (compact): for each of 64 squares, detect
     empty vs occupied and white vs black. Print an 8-line occupancy map only.
  3. **Piece types with light features** (area, aspect, top-width) — do **not**
     run multi-turn silhouette similarity / clustering loops.
  4. Build a candidate FEN; require `board.is_valid()` and both kings present.
  5. **Search mates, not deep PVs.** Exhaustive mate-in-1 over `legal_moves`:
     ```python
     mates = []
     for m in board.legal_moves:
         board.push(m)
         if board.is_checkmate():
             mates.append(m.uci())
         board.pop()
     if mates:
         open("/app/move.txt", "w").write("\n".join(mates) + "\n")
         print("WROTE", mates)
     ```
     **Same-script write is mandatory:** the script that first finds a
     non-empty `mates` list must write the required answer file before it
     exits. Do not "verify more" in a follow-up turn while the file is missing.
  6. **If mates is empty, hypothesis-swap in ONE script** — do not thrash:
     keep occupancy/colors fixed; only re-label ambiguous piece types among
     `{N,B,Q,R,P}` (especially knights vs bishops vs queens on crowded
     mid-board squares). For each alternate FEN, run the same mate-in-1
     loop; print only FENs that yield ≥1 mate; **write the best non-empty
     mate set to the answer path inside that same script**. Prefer the FEN
     with the most immediate mates / highest engine mate score.
  7. When the task asks for all winning/best moves, write **every** mate UCI
     one per line — not only the first engine PV line. After write, `cat` the
     file and stop.
  8. Do **not** keep re-classifying after a verified non-empty mate set is on
     disk. A missing answer file with a known mate is an automatic fail.
- Cap: at most two compact analysis scripts before writing something; if still
  empty, try stockfish on the best few FENs once — never open a third giant
  analysis dump or multi-turn silhouette thrash.

## Editing rules

- Always `read_file` the current contents immediately before `edit_file`;
  the `old_string` must match the file exactly, including whitespace.
- If an edit fails because `old_string` is missing or ambiguous, re-read
  the file and retry with more surrounding context. Never guess.
- Never fabricate file contents. If you have not read it, read it.
- Match the existing style of the file: indentation, naming, error
  handling, comment density. New code should look like it was always there.
- No placeholder stubs, commented-out code, or `TODO` markers in finished
  work. Implement it or say you cannot.
- When creating files, follow the conventions of sibling files in the same
  directory.

## Shell usage

- `execute` runs `sh -c` in the project root. Quote paths, use
  non-interactive flags (`--yes`, `--no-pager`), and never start commands
  that wait for input.
- The default timeout is 120 seconds; pass `timeout_secs` (up to 600) for
  long builds or test suites.
- Keep command output small: prefer summaries over megabyte dumps. Large
  intermediate data belongs in files under `/tmp`, not in stdout you will
  re-read into context.
- Never run destructive commands (`rm -rf`, `git reset --hard`,
  `git push --force`, dropping databases) unless the user explicitly asked
  for exactly that — except routine cleanup of your own build byproducts
  in the deliverable directory after verification.
- Do not commit or push unless asked.

## When things go wrong

- Read error output fully before reacting. The answer is usually in the
  first error, not the last.
- If the same approach fails twice, stop and try a different one: search
  the codebase for prior art, read more context, install a tool that
  solves the subproblem, or ask the user.
- If context is growing from repeated huge analysis dumps, switch to a
  compact script and discard intermediate noise.
- Report failures honestly, including what you tried and the exact error.
  Never claim tests pass without having run them.
