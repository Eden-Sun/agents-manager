/**
 * 一次作答固定在同一個 Run（issue #1879 複審）：讀畫面後 bot 若重啟，文字與 Enter 不能送進新的 Run。
 * 第一次讀到的快照的 `run_id`（舊 daemon 沒有就用 store 裡這顆 bot 目前的 run）就是這次作答的 Run；之後每次重讀
 * 若換了 Run 就放棄（`read` 回 `null`），送鍵一律帶這個 id（store 的 `pinRunId`：不符直接失敗，不走 fresh-run 重試）。
 */
import type { TerminalSnapshot } from '../api/types'

export interface RunPin {
  /** 重讀畫面；讀不到、沒有可固定的 Run、或 Run 已經換了回 `null`。 */
  read: () => Promise<string | null>
  /** 這次作答固定的 Run；還沒讀過或固定不了是 `null`。 */
  id: () => string | null
  /** 讀的時候發現 Run 換了。 */
  changed: () => boolean
}

export function runPin(
  readTerminal: () => Promise<TerminalSnapshot>,
  currentRunId: () => string | null | undefined,
): RunPin {
  let pinned: string | null = null
  let changed = false
  return {
    read: async () => {
      let snap: TerminalSnapshot
      try {
        snap = await readTerminal()
      } catch {
        return null
      }
      const stored = currentRunId() ?? null
      const seen = snap.run_id ?? stored
      if (!seen) return null
      if (pinned === null) pinned = seen
      // 快照與 store 任何一邊顯示換了 Run 都算。
      if (seen !== pinned || (stored !== null && stored !== pinned)) {
        changed = true
        return null
      }
      return snap.text
    },
    id: () => pinned,
    changed: () => changed,
  }
}
