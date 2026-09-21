// `.ts` 副檔名：node --experimental-strip-types 的 resolver 不補副檔名。
import { effortLabel } from '../api/types.ts'
import type { Bot, Run } from '../api/types.ts'

/**
 * 設定與實際在跑的不一致（SPEC §4.4a）。2026-09-09：codex 改設定沒重啟，模型／強度只在啟動時吃，
 * UI 卻只顯示 DB 的設定。`run.runtime_*` 是 daemon 讀回的啟動 argv；`null`＝收編 pane，不比。
 * `runtime_identity` 的空字串是已知的本機預設帳號，`null` 才是未知。
 */
export interface RuntimeDriftField {
  field: 'model' | 'effort' | 'fast' | 'identity'
  label: string
  /** 已翻成人看的字。 */
  running: string
  /** 重啟後會變成的值。 */
  configured: string
}

/** `fast` 只有 codex 會變成啟動旗標，其他 kind 存了也不送。 */
function comparesFast(kind: string): boolean {
  return kind === 'codex'
}

const NONE = '（CLI 預設）'
const DEFAULT_IDENTITY = 'cc0'

function normalizedIdentity(value: string | null | undefined): string | null {
  const trimmed = value?.trim() ?? ''
  return trimmed || null
}

function identityLabel(value: string | null | undefined, kind: string): string {
  return normalizedIdentity(value) ?? (kind === 'claude' ? DEFAULT_IDENTITY : '（預設）')
}

/** 已知的 runtime 身份；`undefined`＝沒記／沒有 active run，`null`＝已知但使用本機預設帳號。 */
export function runtimeIdentity(run: Run | null): string | null | undefined {
  if (!run || (run.state !== 'running' && run.state !== 'starting')) return undefined
  if (typeof run.runtime_identity !== 'string') return undefined
  return normalizedIdentity(run.runtime_identity)
}

/** 模型／強度／fast 這一組是否已知；identity 單獨已知時不能拿 null 猜這三欄。 */
export function runtimeSettingsKnown(run: Run | null): boolean {
  if (!run || (run.state !== 'running' && run.state !== 'starting')) return false
  return run.runtime_model !== null || run.runtime_effort !== null || run.runtime_fast !== null
}

/** runtime 欄位全未知＝收編 pane：別拿 `bots` 設定假裝成 runtime。 */
export function runtimeKnown(run: Run | null): boolean {
  return runtimeSettingsKnown(run) || runtimeIdentity(run) !== undefined
}

export function runtimeDrift(bot: Bot | null, run: Run | null): RuntimeDriftField[] {
  if (!bot || !run || !runtimeKnown(run)) return []
  const out: RuntimeDriftField[] = []
  if (runtimeSettingsKnown(run) && (run.runtime_model ?? null) !== (bot.model ?? null)) {
    out.push({
      field: 'model',
      label: '模型',
      running: run.runtime_model ?? NONE,
      configured: bot.model ?? NONE,
    })
  }
  if (runtimeSettingsKnown(run) && (run.runtime_effort ?? null) !== (bot.effort ?? null)) {
    out.push({
      field: 'effort',
      label: '強度',
      running: run.runtime_effort ? effortLabel(run.runtime_effort) : NONE,
      configured: bot.effort ? effortLabel(bot.effort) : NONE,
    })
  }
  if (comparesFast(bot.kind) && run.runtime_fast !== null && run.runtime_fast !== bot.fast) {
    out.push({ field: 'fast', label: 'fast', running: run.runtime_fast ? '開' : '關', configured: bot.fast ? '開' : '關' })
  }
  if (typeof run.runtime_identity === 'string') {
    const running = normalizedIdentity(run.runtime_identity)
    const configured = normalizedIdentity(bot.identity)
    if (running !== configured) {
      out.push({
        field: 'identity',
        label: '帳號',
        running: identityLabel(running, bot.kind),
        configured: identityLabel(configured, bot.kind),
      })
    }
  }
  return out
}

/** badge 與 tooltip 共用。 */
export function driftLine(d: RuntimeDriftField): string {
  return `${d.label} 實際 ${d.running}、設定 ${d.configured}`
}

export function driftTitle(drift: RuntimeDriftField[]): string {
  if (!drift.length) return ''
  return `${drift.map(driftLine).join('\n')}\n重啟這顆 bot 才會換成設定的值。`
}
