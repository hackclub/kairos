# Notes for AI agents

Kairos records focus time while people build with AI and sends a few facts about it to a server. Hack Club programs read those hours when they give out prizes, so treat the event log the way you'd treat billing code.

## Layout

The README lists the crates. In short: `crates/core` has the event log and the credit rule, `crates/agent` is kairosd, `crates/cli` is the `kairos` command, `crates/server` runs kairos.hackclub.com, and `crates/sim` simulates people against a server you run yourself.

## Building and testing

`cargo test --workspace` runs everything. Server tests need Postgres: set `KAIROS_TEST_DATABASE_URL` and each test gets its own schema. Windows-only code is checked in CI.

## Privacy

Only facts leave a device: times, counts, project names and file paths inside a project. Prompts, replies, code, window titles, URLs, keystrokes and screenshots never do. Keep it that way in any change, and say so in the README's privacy section when an upload starts carrying something new.

## Records

Every event is sealed to the one before it, and events only come from what kairosd sees on the device. Keep it that way: no side doors for writing or replaying events, and the sim stays pointed at local servers.

## Style

Rust everywhere. Server pages are plain HTML, CSS and JS with no framework. Match the code around you, keep comments for things that aren't obvious, and keep changes small.
