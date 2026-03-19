# Robust ZeroClaw Bridge to Telegram 

Small Rust Telegram bot that forwards prompts to a local `zeroclaw` binary and can also run a limited set of host shell commands.

## What It Does

- Accepts Telegram messages from one authorized user.
- Sends plain text prompts to `zeroclaw agent -m "<prompt>"`.
- Downloads Telegram documents and photos into a local workspace directory and passes their paths to ZeroClaw.
- Can send local files and images back into Telegram.
- Supports helper commands such as `/ping`, `/date`, `/free`, `/uptime`, and `/status`.
- Supports direct shell execution with `/sh ...`.
- Serializes requests so it behaves predictably on low-power hardware such as Raspberry Pi 1.

## Security Model

This bot is intentionally minimal and powerful:

- It only trusts one Telegram user ID defined in `TG_ALLOWED_USER_ID`.
- The `/sh` command executes arbitrary shell commands on the host as the bot process user.
- The bot should be used only on a machine you control, in a private chat, with a token you keep secret.

Do not expose this bot to multiple users unless you redesign the authorization model.

## Requirements

- Rust toolchain
- `cross` for Raspberry Pi 1 ARMv6 builds
- A Telegram bot token from BotFather
- A working local `zeroclaw` binary
- Linux shell utilities if you want to use `/date`, `/free`, `/uptime`, `/status`

## Configuration

The bot reads configuration from environment variables:

| Variable | Required | Default | Description |
| --- | --- | --- | --- |
| `TG_BOT_TOKEN` | yes | none | Telegram bot token |
| `TG_ALLOWED_USER_ID` | yes | none | Only this Telegram user ID is allowed to use the bot |
| `ZEROCLAW_BIN` | no | `/home/konst/zeroclaw` | Path to the local `zeroclaw` executable |
| `ZEROCLAW_WORKSPACE_DIR` | no | `$HOME/.zeroclaw/workspace` | Workspace root used to resolve relative files for Telegram uploads |
| `CHAT_STORE_PATH` | no | `./.chat_store.json` | JSON file used to persist per-chat history, recent paths, and remembered facts |
| `ZEROCLAW_TIMEOUT_SEC` | no | `240` | Timeout for both `zeroclaw` and `/sh` commands |

Use `.env.example` as a reference, then export the variables in your shell or define them in your service manager.

To discover `TG_ALLOWED_USER_ID`, you can:

1. Set `TG_ALLOWED_USER_ID=0` temporarily.
2. Start the bot.
3. Send it any message from your Telegram account.
4. Read the stderr log line `unauthorized access attempt: user_id=...`.
5. Restart the bot with that user ID configured.

## Build And Run

Install `cross` if needed:

```bash
cargo install cross --git https://github.com/cross-rs/cross
```

Build for Raspberry Pi 1 ARMv6:

```bash
cross build --release --target arm-unknown-linux-gnueabihf
```

The binary will be written to:

```bash
target/arm-unknown-linux-gnueabihf/release/tg-zeroclaw-bridge
```

Then set the runtime environment and run it on the target machine:

```bash
export TG_BOT_TOKEN="1234567890:your_token"
export TG_ALLOWED_USER_ID="123456789"
export ZEROCLAW_BIN="/home/konst/zeroclaw"
export ZEROCLAW_WORKSPACE_DIR="$HOME/.zeroclaw/workspace"
export CHAT_STORE_PATH="$PWD/.chat_store.json"
export ZEROCLAW_TIMEOUT_SEC="240"
```

For a local native run during development:

```bash
export TG_BOT_TOKEN="1234567890:your_token"
export TG_ALLOWED_USER_ID="123456789"
export ZEROCLAW_BIN="/home/konst/zeroclaw"
export ZEROCLAW_WORKSPACE_DIR="$HOME/.zeroclaw/workspace"
export CHAT_STORE_PATH="$PWD/.chat_store.json"
export ZEROCLAW_TIMEOUT_SEC="240"

cargo run --release
```

If you want it to stay running on a server or Raspberry Pi, run the compiled binary under `systemd --user`, `tmux`, or another supervisor.

## Telegram Commands

| Command | Description |
| --- | --- |
| `/ping` | Health check |
| `/id` | Show your Telegram user ID |
| `/raw <prompt>` | Send prompt directly to ZeroClaw |
| `/ask <prompt>` | Same as `/raw` |
| `/sh <command>` | Run a shell command on the host |
| `/ls [path]` | Run `ls -la` for the current directory or a provided path |
| `/cat <path>` | Print a local file with `cat` |
| `/date` | Run `date` |
| `/free` | Run `free -h` |
| `/uptime` | Run `uptime` |
| `/status` | Run `systemctl --user status zeroclaw -n 40 --no-pager` |
| `/sendfile <path>` | Send a local file into Telegram |
| `/sendphoto <path>` | Send a local image into Telegram as a photo |
| `/help` | Show built-in command help |

Any non-command text message is also forwarded to ZeroClaw.

## Notes

- Long replies are split into Telegram-sized chunks.
- ANSI escape sequences are stripped from command output.
- Bridge-mode prompts include the last 10 stored chat messages for that `chat_id` as context.
- The bot persists recent chat history, recent file references, and simple remembered facts in `CHAT_STORE_PATH`.
- Simple user facts like `my favorite color is red` are captured and injected into future bridge prompts.
- That history is used for normal chat and `/ask`, but not for `/raw`, `/sh`, `/ls`, or `/cat`.
- Telegram documents and photos are stored under `.telegram_uploads/` so ZeroClaw can inspect them with local tools.
- Relative upload paths are resolved against both the bot working directory and `ZEROCLAW_WORKSPACE_DIR`.
- ZeroClaw can request a real Telegram upload by emitting one of these lines on its own line:
  `[[telegram_document:relative/or/absolute/path|optional caption]]`
  `[[telegram_photo:relative/or/absolute/path|optional caption]]`
- Some interactive shell commands are blocked and replaced with a hint.
- Unauthorized access attempts are logged to stderr.

## License

MIT. See `LICENSE`.
