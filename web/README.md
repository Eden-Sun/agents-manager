# agents-manager web UI

**English** | [繁體中文](README.zh-TW.md)

React + TypeScript + Vite + Zustand front end for `agents-managerd`. It only projects daemon state; it never talks to agent processes.

See [`../docs/FRONTEND.md`](../docs/FRONTEND.md) for structure, mock mode and conventions.

```bash
bun install
bun run dev              # connect to the real daemon (127.0.0.1:7788); start it first with `cargo run -p agents-managerd -- serve`
VITE_MOCK=1 bun run dev  # in-memory fake backend, no daemon needed
bun run build            # output in dist/, embedded into the daemon binary at its next build
bun run lint
```
