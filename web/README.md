# agents-manager web UI

**English** | [繁體中文](README.zh-TW.md)

React + TypeScript + Vite + Zustand front end for `agents-managerd` (M7 in SPEC Appendix D).

See [`../docs/FRONTEND.md`](../docs/FRONTEND.md) for full details, mock mode and known issues.

```bash
bun install
bun run dev              # connect to the real daemon (127.0.0.1:7788); run cargo run -- serve first
VITE_MOCK=1 bun run dev  # in-memory fake backend, no daemon needed
bun run build            # output in dist/, embedded via rust-embed (M8)
bun run lint
```
