import type { BotKind } from '../api/types'

/**
 * codex 的裝置碼登入（`codex login --device-auth`）一啟動就刪掉那個 `CODEX_HOME` 的 `auth.json`
 * （2026-11-03 拋棄式目錄實測：啟動後幾秒檔案就沒了，取消也救不回來）。所以**已登入**的 codex 身份照登會立即失效，要先問。
 * 未登入的沒有東西可以失效，直接開始。claude 的登入流程不走這條。
 */
export function codexLoginReplacesLogin(kind: BotKind, loggedIn: boolean | null | undefined): boolean {
  return kind === 'codex' && loggedIn === true
}
