# Messaging gateway

Run Wizard headless as a chat bot: each inbound message drives one autonomous agent turn in your project, and the reply comes back in the chat. Telegram is the supported transport.

```bash
cd ~/your/project
wizard --gateway
```

The gateway is a **long-running foreground process** (Ctrl-C stops it). **Nothing listens until this process is running** — that is the #1 reason Telegram messages get no reply after onboarding. It uses the current working directory (or `$WIZARD_GATEWAY_CWD`) as the project root and builds **one** agent for the whole session, so the conversation continues across messages. It runs in sovereign posture: no terminal, no human in the loop, tool calls execute directly. Read [SECURITY.md](../SECURITY.md) before pointing a public bot at a machine you care about.

## Setup

1. Create a bot with [@BotFather](https://t.me/BotFather) and copy the token.
2. Give Wizard the token (checked in this order):
   - **Preferred:** paste it during `wizard --onboard` when you pick Telegram — it is stored under `telegram` in `~/.wizard/credentials.toml` (file mode 0600).
   - Or write it yourself:

     ```toml
     # ~/.wizard/credentials.toml  (chmod 600)
     [keys]
     telegram = "123456:ABC-..."
     ```

   - Or export it in the gateway's environment (`WIZARD_TELEGRAM_TOKEN` by default). The env var is only used when no credential is stored.
3. Add a `[gateway]` section to `~/.wizard/config.toml`, or pick Telegram in onboarding (`wizard --onboard`), which writes the same thing:

```toml
[gateway]
kind = "telegram"
allowed_chat_ids = [123456789]
```

4. **Start the gateway and keep it running:**

```bash
cd ~/your/project
wizard --gateway
```

## Always-on: systemd user unit

The gateway does not daemonize itself. To keep it running across logouts and reboots, install a systemd **user** unit.

Copy the unit from the repo (or paste the block below):

```bash
mkdir -p ~/.config/systemd/user
cp contrib/wizard-gateway.service ~/.config/systemd/user/
# Edit WorkingDirectory (or set WIZARD_GATEWAY_CWD) to your project:
#   systemctl --user edit wizard-gateway
#   [Service]
#   WorkingDirectory=%h/your/project
#   # or: Environment=WIZARD_GATEWAY_CWD=%h/your/project
systemctl --user daemon-reload
systemctl --user enable --now wizard-gateway
journalctl --user -u wizard-gateway -f
```

Unit contents (`contrib/wizard-gateway.service`):

```ini
[Unit]
Description=Wizard Telegram gateway
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
WorkingDirectory=%h
ExecStart=%h/.local/bin/wizard --gateway
Restart=on-failure
RestartSec=5
Environment=RUST_LOG=info
# Environment=WIZARD_GATEWAY_CWD=%h/your/project

[Install]
WantedBy=default.target
```

If the binary lives elsewhere (e.g. `/usr/local/bin/wizard`), set `ExecStart` accordingly. Enable lingering so the user service survives logout: `loginctl enable-linger $USER`.

## `[gateway]` config keys

| Key | Default | Meaning |
|-----|---------|---------|
| `kind` | `"none"` | Which transport to run: `"none"` (terminal only; `--gateway` errors with instructions) or `"telegram"` |
| `token_env` | `"WIZARD_TELEGRAM_TOKEN"` | Name of the env var holding the bot token, consulted when no `telegram` entry exists in `~/.wizard/credentials.toml`. Only the *name* is stored; the token itself is never persisted to config |
| `allowed_chat_ids` | `[]` | Inbound chat IDs allowed to drive the agent. **Empty means allow all**: set it. Unauthorized chats get an "unauthorized" reply and nothing runs |

To find your chat ID, message the bot once and read the `chat.id` from `https://api.telegram.org/bot<token>/getUpdates`.

## Behavior

- **Transport.** Long-polls `getUpdates` (30 s window, `allowed_updates=["message"]`) and replies via `sendMessage`. Sends a `typing` chat action while the agent turn runs. Transient network errors are retried with exponential backoff (`retry_base_secs` / `retry_max_secs` from the top-level config).
- **One turn per message.** Each text, caption, photo, or image-document message runs a full agent turn (tools, file edits, shell) and the final response is sent back. Photos/documents are downloaded under `~/.wizard/gateway-attachments/` and the agent prompt includes `[attached: /absolute/path]`. Photo-only messages use the prompt `Please look at the attached image.` Stickers, voice, and other unsupported types get a short "unsupported message type" reply instead of silence. Replies are capped at 24,000 characters and split into Telegram-sized chunks (≤ 4,000 characters, breaking on line boundaries).
- **In-chat commands.** `/plan` toggles plan mode (the next task is planned read-only first) and `/omakase` toggles chef's-choice mode, mirroring the TUI commands. Other slash commands are not available over the gateway.
- **Step budget.** The gateway runs in sovereign posture, so a *capped* `max_steps` below 100 is raised to 100 — there is no human to hand back to mid-task. The default (`max_steps = 0`, no limit) is already more permissive and is left alone: a turn runs until the model stops calling tools.

## Diagnose with `wizard doctor`

When `gateway.kind = "telegram"`, doctor reports:

- gateway kind
- whether a telegram token is present (credentials or env — never prints the secret)
- whether a `wizard --gateway` process appears to be running

It also fails when a telegram token is stored but `gateway.kind` is still `"none"`.

```bash
wizard doctor
```
