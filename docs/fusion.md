# Fusion: `/fusion`

Model fusion runs several providers as a debate panel, then a synthesizer merges
their answers into one. A *panel* of providers each answer the turn independently
and critique each other over N rounds; a *synthesizer* then produces the final,
tool-capable answer with the panel's drafts as guidance. In practice the fusion
of independent models tends to beat the best single model in the panel, since
they catch each other's mistakes.

Fusion is just another provider under the hood (a `FusionProvider` implementing
the same `LlmProvider` trait), so the agent loop, tools, modes, and streaming are
unchanged. `/fusion` simply swaps the active provider to the panel and back.

```
/fusion          toggle fusion on/off
/fusion config   choose which providers form the panel
```

The debate engine is the standalone [FUSION](https://github.com/teddytennant/FUSION)
project, consumed here as the provider-agnostic `fusion-core` crate.

## How a fused turn works

1. **Panel (advisors).** Each panel provider answers the conversation as plain
   text and critiques the others over `rounds` review rounds. Panel members get
   no tools — they only advise.
2. **Synthesis (the actor).** The synthesizer receives the real request *with its
   tools*, plus the panel's drafts injected as guidance, and produces the final
   streamed answer. It is the **sole tool-caller**, so fusion works on agentic
   turns, not just Q&A — there are never conflicting tool calls.

## Configuring the panel

`/fusion config` opens a multi-select: Space toggles a provider into the panel,
Enter saves. The **first** toggled provider becomes the synthesizer. Panel
members are existing entries from `/provider` — each provider already binds a
model, so a panel member *is* a registered provider.

This writes `[fusion]` to `~/.wizard/config.toml`:

```toml
[fusion]
panel = ["claude", "openrouter"]   # provider names from [[providers]]
synthesizer = "claude"             # the sole tool-caller
rounds = 1                         # critique rounds (edit here; default 1)
```

If `[fusion]` is unset, `/fusion` derives a default panel from your first two
configured providers (synthesizer = the first). With a single provider it
degrades to a passthrough rather than erroring. `rounds` has no UI — edit it in
the config file (default 1, which is usually the sweet spot).

## Cost

Fusion is expensive: each turn makes `panel × (1 + rounds)` advisory calls plus
one synthesis call — several times the tokens of a single-model turn. While it is
on, the status bar shows the fusion label (e.g. `fusion: claude+openrouter ×1`)
in a loud accent style so it is never left running unnoticed. Toggling fusion, like
switching providers, resets the session.
