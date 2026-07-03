# Messaging gateway

Run Wizard headless as a chat bot: each inbound message drives one autonomous agent turn in your project, and the reply comes back in the chat. Telegram is the supported transport.

```bash
cd ~/your/project
export WIZARD_TELEGRAM_TOKEN=123456:ABC-...
wizard --gateway
```

The gateway is a long-running foreground process (Ctrl-C stops it). It uses the current working directory as the project root, builds **one** agent for the whole session — so the conversation continues across messages — and runs in sovereign posture: no terminal, no human in the loop, tool calls execute directly. Read [SECURITY.md](../SECURITY.md) before pointing a public bot at a machine you care about.

## Setup

1. Create a bot with [@BotFather](https://t.me/BotFather) and copy the token.
2. Give Wizard the token — either store it under `telegram` in `~/.wizard/credentials.toml` (file mode 0600; checked first, and works for a gateway launched from cron with no environment), or export it in the gateway's environment (`WIZARD_TELEGRAM_TOKEN` by default). It is read at startup and never written to `config.toml`.
3. Add a `[gateway]` section to `~/.wizard/config.toml` — or pick Telegram in onboarding (`wizard --onboard`), which writes the same thing:

```toml
[gateway]
kind = "telegram"
allowed_chat_ids = [123456789]
```

4. Start it: `wizard --gateway`.

## `[gateway]` config keys

| Key | Default | Meaning |
|-----|---------|---------|
| `kind` | `"none"` | Which transport to run: `"none"` (terminal only; `--gateway` errors with instructions) or `"telegram"` |
| `token_env` | `"WIZARD_TELEGRAM_TOKEN"` | Name of the env var holding the bot token, consulted when no `telegram` entry exists in `~/.wizard/credentials.toml`. Only the *name* is stored; the token itself is never persisted to config |
| `allowed_chat_ids` | `[]` | Inbound chat IDs allowed to drive the agent. **Empty means allow all** — set it. Unauthorized chats get an "unauthorized" reply and nothing runs |

To find your chat ID, message the bot once and read the `chat.id` from `https://api.telegram.org/bot<token>/getUpdates`.

## Behavior

- **Transport.** Long-polls `getUpdates` (30 s window) and replies via `sendMessage`. Transient network errors are retried with exponential backoff (`retry_base_secs` / `retry_max_secs` from the top-level config).
- **One turn per message.** Each text message runs a full agent turn — tools, file edits, shell — and the final response is sent back. Replies are capped at 24,000 characters and split into Telegram-sized chunks (≤ 4,000 characters, breaking on line boundaries).
- **In-chat commands.** `/plan` toggles plan mode (the next task is planned read-only first) and `/omakase` toggles chef's-choice mode, mirroring the TUI commands. Other slash commands are not available over the gateway.
- **Step budget.** The gateway raises `max_steps` to at least the sovereign default, since there is no human to hand back to mid-task.
