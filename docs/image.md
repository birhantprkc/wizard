# Image generation

The native `generate_image` tool creates images from a text prompt via an OpenAI-compatible `POST {base}/images/generations` endpoint. On xAI that is the Imagine API (`https://api.x.ai/v1/images/generations`). The result is always written to a local file so the agent (and you) can open it.

## generate_image

- **Arguments**
  - `prompt` (required) — image description
  - `path` (optional) — save location, relative to the project root or absolute. Default: `generated/<timestamp>-<slug>.png` under the project root (the directory is created as needed)
  - `model` (optional) — defaults to `grok-imagine-image-quality` on xAI hosts, or `dall-e-3` when the active provider is OpenAI/OpenRouter
  - `n` (optional, 1–4, default 1) — number of images; multi-image saves get `-1`, `-2`, … suffixes
  - `aspect_ratio` (optional) — xAI Imagine ratios such as `1:1`, `16:9`, `9:16`, `4:3`, `3:4`, …
  - `resolution` (optional) — xAI Imagine `1k` or `2k`
  - `response_format` (optional) — `url` (default; download to file) or `b64_json` (decode to file)
- **Access:** execute (network + disk). Not available in plan mode.
- Temporary CDN URLs from `response_format=url` are downloaded over HTTPS only; local/private hosts are refused.

### Auth resolution

1. If the **active provider** is `xai` / `xaioauth` / `openai` / `openrouter`, use its `base_url` and token (OAuth for `xaioauth`, stored credentials / env key otherwise).
2. Else fall back to an xAI OAuth session (`/login xai`), then a stored `xai` key, then `XAI_API_KEY`, against `https://api.x.ai/v1`.
3. If nothing is configured, the tool returns a clear error asking you to `/login xai` or set a key.

xAI models of note: `grok-imagine-image-quality` (default) and `grok-imagine-image`.

## Example

Ask the agent to generate an image, or call the tool directly in a turn:

```text
generate a 16:9 concept art of a lighthouse at dusk
```

Files land under `generated/` in the project root unless you pass `path`.
