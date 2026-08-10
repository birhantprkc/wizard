# Fusion: `/fusion`

A council of providers. Fusion runs several providers as a debate panel, then a
synthesizer merges their answers into one. Each panel provider answers the turn
independently and critiques the others over N rounds. The synthesizer then
produces the final, tool-capable answer, using the panel's drafts as guidance.
Independent models catch each other's mistakes in the critique rounds.

Fusion is just another provider under the hood (a `FusionProvider` implementing
the same `LlmProvider` trait), so the agent loop, tools, modes, and streaming are
unchanged. `/fusion` swaps the active provider to the panel and back.

```
/fusion          toggle fusion on/off
/fusion config   choose which providers form the panel
```

The debate engine started as the standalone
[FUSION](https://github.com/teddytennant/FUSION) project and lives here in
`src/llm/fusion.rs`. It is no longer a dependency: `cargo publish` refuses a
crate that carries a `{ git = ... }` dependency, and Wizard keeps the option of
being publishable open. (It is not published to crates.io today.)

The fan-out itself is no longer written here. `/fusion` and [`/ultra`](ultra.md)
are one primitive (fan out N candidates, adjudicate, hand the result to the one
thing that acts) configured two ways: fusion's candidates are providers and its
adjudicator is the critique rounds; ultra's candidates are lens subagents and its
adjudicator is a judge. What stays in `src/llm/fusion.rs` is what is specific to
being a *provider*: which model synthesizes, how the conversation is flattened
for members that see no structured history, the guidance the synthesizer gets,
and the run log.

## How a fused model call works

A fused provider is consulted once per agent *step*, not once per turn, so an
agentic turn that makes N tool calls runs the whole thing below N times.

1. **Panel (advisors).** Each panel provider answers the conversation as plain
   text and critiques the others over `rounds` review rounds. Panel members get
   no tools; they only advise. A member that fails contributes nothing to the
   critique round but is still *named* in the synthesizer's guidance with an
   empty body, so a wholly dead panel is visible instead of silently degrading
   the turn to a plain single-model answer. Each member call is capped at five
   minutes, so one throttled provider cannot park the synthesis behind it
   indefinitely.
2. **Synthesis (the actor).** The synthesizer receives the real request *with its
   tools*, plus the panel's drafts injected as guidance, and produces the final
   streamed answer. It is the **sole tool-caller**, so fusion works on agentic
   turns, not just Q&A. There are never conflicting tool calls.

## Running it with `/ultra`

They stack, and each no longer refuses to turn on over the other. With both on,
the ultra lens roster is dealt across this panel's providers round-robin, so a
candidate talks to one panel provider directly instead of re-running the whole
debate for its own draft. See [Running both](ultra.md#running-both).

## Configuring the panel

`/fusion config` opens a multi-select: Space toggles a provider into the panel,
Enter saves. The **first** toggled provider becomes the synthesizer. Panel
members are existing entries from `/provider`: each provider already binds a
model, so a panel member *is* a registered provider.

This writes `[fusion]` to `~/.wizard/config.toml`:

```toml
[fusion]
panel = ["claude", "openrouter"]   # provider names from [[providers]]
synthesizer = "claude"             # the sole tool-caller
rounds = 1                         # critique rounds (edit here; default 1)
```

A panel or synthesizer naming a provider you have not configured is refused when
fusion is built (`fusion references unknown provider '<name>'`). `rounds` is not
validated: `rounds = 0` just means no critique round at all.

"First" means first in the `[[providers]]` list, not the first key you pressed.

If `[fusion]` is unset, `/fusion` derives a default panel from your configured
providers: the first two when you have two or more (synthesizer = the first),
or a one-member panel when you only have one. With no providers at all it
derives nothing and `/fusion` says so. An empty `panel = []` is the real
passthrough case (synthesizer alone, no debate); the derivation never produces
one. `rounds` has no UI: edit it in the config file (default 1, usually the
sweet spot).

## Cost

Fusion is expensive: each *step* makes `panel × (1 + rounds)` advisory calls plus
one synthesis call, so a turn that calls N tools pays that N times — several
times the tokens of a single-model turn, and more on agentic work. While it is
on, the status bar shows the fusion label (e.g. `fusion: claude+openrouter ×1`)
in a loud accent style, so it's never left running unnoticed. Toggling fusion,
like switching providers, resets the session.

With `/ultra` also on, the notice appends the seated ultra label
(`· ultra ×3 · … · across claude+openrouter`), because the two together are the
most expensive thing Wizard will do on your behalf and the cost has to be
readable before the turn, not after it.
