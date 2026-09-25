import type { RestartBatch } from '../api/types'
import { CLI_UPDATE_PHASE_LABEL, type CliUpdate } from '../store/cliUpdate'

/** 合成那顆的兩項選單文案與按鈕的 aria-label（純函式，好測）。 */
export function mergedUpdateLabels(x: {
  batch: RestartBatch | null
  cli: CliUpdate | null
  readyCount: number
  busyCount: number
  installCount: number
  to: string | null
}): { restartItem: string; codexItem: string; label: string } {
  const { batch, cli, readyCount, busyCount, installCount, to } = x
  const restartItem = batch
    ? batch.finished
      ? `重啟完成 · 成功 ${batch.ok.length} 顆${batch.failed.length ? ` · 失敗 ${batch.failed.length} 顆` : ''}（收起）`
      : `重啟中 ${batch.done}/${batch.total}（收起，不會中斷）`
    : readyCount > 0
      ? `重啟 ${readyCount} 顆閒置的 Bot 套用更新`
      : `${busyCount} 顆有更新但在忙，閒下來再按`
  const codexItem = cli
    ? `${cli.host} 的 codex ${CLI_UPDATE_PHASE_LABEL[cli.phase]}…`
    : `安裝 codex${to ? ` ${to}` : ''} 並重啟（${installCount} 顆還沒裝）`
  return { restartItem, codexItem, label: `有更新：${restartItem}；${codexItem}。點開選要做哪一個` }
}
