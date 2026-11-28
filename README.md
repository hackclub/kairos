# Kairos

Kairos tracks your focus time while you build with AI: writing prompts, reading what the agent did, fixing code and testing your app.

## Install

Windows (PowerShell):

```
irm https://kairos.hackclub.com/install.ps1 | iex
```

macOS and Linux:

```
curl -fsSL https://kairos.hackclub.com/install.sh | sh
```

It installs for your user only, starts when you log in, and opens your browser to connect your account. Open a new terminal afterwards to use the `kairos` command.

## Works with

Claude Code, Codex, Cursor, GitHub Copilot, Antigravity, Gemini CLI, Cline, Roo Code, Kilo, opencode, Qwen Code, Pi and Hermes Agent, plus any editor or terminal. An optional browser extension counts the time you spend testing your app on localhost.

After install, WakaTime editor plugins send their heartbeats to Kairos, and Kairos passes every heartbeat on to where they went before (Hackatime for most people). Editors that have no WakaTime or Hackatime plugin yet get Hackatime's, and `kairos uninstall` takes out only the plugins Kairos added.

## Privacy

Kairos sends times, counts, project names and which files in a project you worked on. Your code, prompts, the agent's replies, window titles and tab addresses stay on your computer, and Kairos doesn't take screenshots. The server also notes the country and network provider your uploads come from. With token stats on, it also sends the token counts your AI tools log and which model each was for.

- `kairos exclude <folder>` keeps a folder out completely.
- `kairos pause [30m|1h|2h|tomorrow]` pauses tracking, `kairos resume` starts it again.
- `kairos uninstall` removes Kairos and keeps what it saved on your computer.

## For programs

The read API follows Hackatime's public API, so if your program reads from Hackatime, you can switch to Kairos by changing the base URL.

## Self-hosting

Copy `deploy/.env.example` to `deploy/.env`, fill it in, and run `deploy/deploy.sh`. It builds the server and starts it with Postgres through Docker Compose. Sign-in uses Hack Club Auth.

## Development

```
crates/core       events, credit rules, AI tool log readers
crates/agent      kairosd, the background app
crates/cli        kairos, the command line
crates/server     web app and API
crates/sim        simulated users for testing the credit rules
crates/release    signs release builds
extension/        browser extension
packaging/        install scripts, GNOME Shell extension
```

```
KAIROS_TEST_DATABASE_URL=postgres://user:pass@localhost/db cargo test --workspace
```

The server tests create a schema per test in that database.

## License

AGPL-3.0
