---
name: coding
description: Baseline coding workflow — explore before editing, make minimal precise diffs, verify with the project's own build and tests, and review the diff before finishing.
---

# Coding guidelines

## Workflow

1. **Orient first.** Before changing anything, build a picture of the code:
   use `list_files` to see the layout, `search_files` to find relevant
   symbols, and `read_file` to read the code you are about to touch. Check
   `git_status` so you know the starting state of the working tree.
2. **Honor project instructions.** If the repo has an `AGENTS.md` or
   `WIZARD.md`, its build/test commands, style rules, and forbidden
   directories override your defaults.
3. **Plan briefly, then act.** State what you intend to change and why in a
   sentence or two, then make the change. Do not narrate every tool call.
4. **Make the smallest change that solves the task.** Prefer `edit_file`
   with an exact, unique `old_string` over rewriting whole files with
   `write_file`. Do not reformat, rename, or "clean up" code you were not
   asked to touch.
5. **Verify every change.** Run the project's own commands via `execute` —
   from `AGENTS.md` when present, otherwise infer from the repo
   (`Cargo.toml` → `cargo check` / `cargo test`, `package.json` →
   `npm test`, `pyproject.toml` → `pytest`, `Makefile` → `make test`).
6. **Review before declaring done.** Run `git_diff` and read your own
   changes. Confirm they compile, pass tests, and contain nothing
   unintended.

## Editing rules

- Always `read_file` the current contents immediately before `edit_file`;
  the `old_string` must match the file exactly, including whitespace.
- If an edit fails because `old_string` is missing or ambiguous, re-read
  the file and retry with more surrounding context — never guess.
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
- Never run destructive commands (`rm -rf`, `git reset --hard`,
  `git push --force`, dropping databases) unless the user explicitly asked
  for exactly that.
- Do not commit or push unless asked.

## When things go wrong

- Read error output fully before reacting; the answer is usually in the
  first error, not the last.
- If the same approach fails twice, stop and try a different one — search
  the codebase for prior art, read more context, or ask the user.
- Report failures honestly, including what you tried and the exact error.
  Never claim tests pass without having run them.
