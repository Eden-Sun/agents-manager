import type { RestartBatch } from '../api/types'
import { CLI_UPDATE_PHASE_LABEL, type CliUpdate } from '../store/cliUpdate'

/** 合成那顆的兩項選單文案與按鈕的 aria-label（純函式，好測）。 */
export function mergedUpdateLabels(x: {
  batch: RestartBatch | null
  cli: CliUpdate[] | CliUpdate | null
  readyCount: number
  busyCount: number
  installCount: number
  to: string | null
  claudeInstallCount?: number
  claudeTarget?: string | null
  claudeSummary?: string
  claudeShown?: boolean
  restartShown?: boolean
  codexShown?: boolean
}): { restartItem: string; codexItem: string; claudeItem: string; label: string } {
  const { batch, readyCount, busyCount, installCount, to } = x
  const updates = Array.isArray(x.cli) ? x.cli : x.cli ? [x.cli] : []
  const codexCli = updates.find((item) => item.kind === 'codex')
  const claudeCli = updates.filter((item) => item.kind === 'claude')
  const restartItem = batch
    ? batch.finished
      ? `重啟完成 · 成功 ${batch.ok.length} 顆${batch.failed.length ? ` · 失敗 ${batch.failed.length} 顆` : ''}（收起）`
      : `重啟中 ${batch.done}/${batch.total}（收起，不會中斷）`
    : readyCount > 0
      ? `重啟 ${readyCount} 顆閒置的 Bot 套用更新`
      : `${busyCount} 顆有更新但在忙，閒下來再按`
  const codexItem = codexCli
    ? `${codexCli.host} 的 codex ${CLI_UPDATE_PHASE_LABEL[codexCli.phase]}…`
    : `安裝 codex${to ? ` ${to}` : ''} 並重啟（${installCount} 顆還沒裝）`
  const claudeItem = claudeCli.length
    ? `claude 安裝中 ${claudeCli.map((item) => `${item.host} ${CLI_UPDATE_PHASE_LABEL[item.phase]}`).join('、')}…`
    : `安裝 claude${x.claudeTarget ? ` ${x.claudeTarget}` : ''} 到 ${x.claudeInstallCount ?? 0} 台`
  const pieces: string[] = []
  if (x.restartShown !== false) pieces.push(restartItem)
  if (x.codexShown !== false) pieces.push(codexItem)
  if (x.claudeShown) pieces.push(claudeItem)
  if (x.claudeSummary && x.claudeShown) pieces.push(x.claudeSummary)
  return { restartItem, codexItem, claudeItem, label: `有更新：${pieces.join('；')}。點開選要做哪一個` }
}
