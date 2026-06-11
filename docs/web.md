# Web tools

Two native tools give the agent web access: `web_fetch` (read a page) and `web_search` (query a search engine). Both are read-only, so they stay available in plan mode. Settings live in the `[web]` section of `~/.wizard/config.toml`.

## web_fetch

Fetch a URL over HTTP(S) and return its content.

- **Arguments:** `url` (required), `max_bytes` (optional cap on response bytes read; clamped to the config cap)
- HTML pages are converted to markdown; other text content types (plain text, JSON, XML, ...) are returned as-is; binary content is summarized, not dumped
- Sends a desktop browser user agent, follows redirects (max 10), 30-second timeout
- The response body is read up to `fetch_max_bytes` (default 100 000) and marked when capped

### SSRF guard

By default, `web_fetch` refuses to touch the local network. A request is rejected when its host:

- is a literal loopback, private, or link-local address (`127.0.0.0/8`, `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`, `169.254.0.0/16`, and the IPv6 equivalents `::1`, `fc00::/7`, `fe80::/10`)
- is `localhost` or a `*.local` mDNS name
- resolves via DNS to any of the above ranges

Redirects are re-checked hop by hop. Non-`http(s)` schemes are always rejected. To fetch from your own LAN or a local dev server, set `allow_local = true`.

## web_search

Query a search backend and return a numbered markdown list of results (title, url, snippet).

- **Arguments:** `query` (required), `count` (optional, default 5, max 10)

Three backends, selected by `search_backend`:

| Backend | Key needed | How |
|---------|-----------|-----|
| `duckduckgo` (default) | none | scrapes the DuckDuckGo HTML endpoint |
| `brave` | yes | Brave Search API (`X-Subscription-Token`) |
| `tavily` | yes | Tavily Search API |

For the keyed backends, set `search_api_key_env` to the **name** of the environment variable holding the key. The key itself is never written to config or disk; it is read from the environment at call time.

## Configuration

```toml
[web]
fetch_max_bytes = 100000          # cap on web_fetch response bytes (default 100000)
allow_local = false               # permit localhost/private-range fetches (default false)
search_backend = "duckduckgo"     # "duckduckgo" | "brave" | "tavily"
search_api_key_env = "BRAVE_API_KEY"  # env var name holding the search key (keyed backends)
```

Every key is optional; a missing `[web]` section means the defaults above.
