# agents-manager web UI

React + TypeScript + Vite + Zustand front end for `agents-managerd` (SPEC 附錄 D 的 M7）。

完整說明、mock 模式與已知問題見 [`../docs/FRONTEND.md`](../docs/FRONTEND.md)。

```bash
npm install
npm run dev              # 連真 daemon（127.0.0.1:7788），需先 cargo run -- serve
VITE_MOCK=1 npm run dev  # 記憶體假後端，不需要 daemon
npm run build            # 產物在 dist/，M8 由 rust-embed 內嵌
npm run lint
```
