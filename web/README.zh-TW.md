# agents-manager web UI

[English](README.md) | **繁體中文**

`agents-managerd` 的 React + TypeScript + Vite + Zustand 前端。它只投影 daemon 狀態，不直接碰 agent 程序。

結構、mock 模式與約定見 [`../docs/FRONTEND.md`](../docs/FRONTEND.md)。

```bash
bun install
bun run dev              # connect to the real daemon (127.0.0.1:7788); start it first with `cargo run -p agents-managerd -- serve`
VITE_MOCK=1 bun run dev  # in-memory fake backend, no daemon needed
bun run build            # output in dist/, embedded into the daemon binary at its next build
bun run lint
```
