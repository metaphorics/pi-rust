# pi-rust

Greenfield Rust rewrite of the [pi](https://github.com/earendil-works/pi) agent harness (`@earendil-works/pi-coding-agent`). The `pi` binary is a drop-in replacement for an existing pi install (same `~/.pi` config/session/auth formats). Unmodified TypeScript extensions keep working via an on-demand Bun sidecar; the TUI is built on [inkferro](https://github.com/metaphorics/inkferro) (git submodule at `./inkferro`).

## Build

```bash
git submodule update --init --recursive
cargo build --workspace
```

Submodule remote: `https://github.com/metaphorics/inkferro` (pinned at the commit recorded in this repo).

Sidecar (optional until extensions are used):

```bash
cd sidecar && bun install
```
