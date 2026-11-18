import type { RestartBatch } from '../api/types'
import { t, tr } from './i18n'
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
  /** herdr 那一項（`herdrMenuItem`）；沒有就不列。 */
  herdrItem?: string | null
  /** grok 那一項（只提示）；沒有就不列。 */
  grokItem?: string | null
  /** agy 那一項（有一鍵安裝，issue #1921）；沒有就不列。 */
  agyItem?: string | null
}): { restartItem: string; codexItem: string; claudeItem: string; label: string } {
  const { batch, readyCount, busyCount, installCount, to } = x
  const updates = Array.isArray(x.cli) ? x.cli : x.cli ? [x.cli] : []
  const codexCli = updates.find((item) => item.kind === 'codex')
  const claudeCli = updates.filter((item) => item.kind === 'claude')
  const restartItem = batch
    ? batch.finished
      ? t('重啟完成 · 成功 {ok} 顆{failed}（收起）', {
          ok: batch.ok.length,
          failed: batch.failed.length ? t(' · 失敗 {n} 顆', { n: batch.failed.length }) : '',
        })
      : t('重啟中 {done}/{total}（收起，不會中斷）', { done: batch.done, total: batch.total })
    : readyCount > 0
      ? t('重啟 {n} 顆閒置的 Bot 套用更新', { n: readyCount })
      : t('{n} 顆有更新但在忙，閒下來再按', { n: busyCount })
  const codexItem = codexCli
    ? t('{host} 的 codex {phase}…', { host: codexCli.host, phase: tr(CLI_UPDATE_PHASE_LABEL[codexCli.phase]) })
    : t('安裝 codex{to} 並重啟（{n} 顆還沒裝）', { to: to ? ` ${to}` : '', n: installCount })
  const claudeItem = claudeCli.length
    ? t('claude 安裝中 {hosts}…', { hosts: claudeCli.map((item) => `${item.host} ${tr(CLI_UPDATE_PHASE_LABEL[item.phase])}`).join(t('、')) })
    : t('安裝 claude{target} 到 {n} 台', { target: x.claudeTarget ? ` ${x.claudeTarget}` : '', n: x.claudeInstallCount ?? 0 })
  const pieces: string[] = []
  if (x.restartShown !== false) pieces.push(restartItem)
  if (x.codexShown !== false) pieces.push(codexItem)
  if (x.claudeShown) pieces.push(claudeItem)
  if (x.claudeSummary && x.claudeShown) pieces.push(x.claudeSummary)
  if (x.herdrItem) pieces.push(x.herdrItem)
  if (x.grokItem) pieces.push(x.grokItem)
  if (x.agyItem) pieces.push(x.agyItem)
  return { restartItem, codexItem, claudeItem, label: t('有更新：{items}。點開選要做哪一個', { items: pieces.join(t('；')) }) }
}
