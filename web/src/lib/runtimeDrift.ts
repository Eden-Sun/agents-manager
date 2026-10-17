// `.ts` 副檔名：node --experimental-strip-types 的 resolver 不補副檔名。
import { effortLabel } from '../api/types.ts'
import type { Bot, Run } from '../api/types.ts'

/**
 * 設定與實際在跑的不一致（SPEC §4.4a）。2026-09-09：codex 改設定沒重啟，模型／強度只在啟動時吃，
 * UI 卻只顯示 DB 的設定。現況：`run.runtime_*` 不只是啟動 argv，畫面巡邏會持續讀 TUI 的狀態列校正，
 * 所以 TUI 裡手改的值也會進來。欄位語義：
 * - `runtime_model`／`runtime_effort` 是 `null`＝未知（沒讀到、收編 pane），那一欄不比不標，其他有值的欄位照比。
 * - `runtime_effort` 是 `'default'`＝畫面明確寫 CLI 預設，是已知的值，跟設定 `null` 同義；跟具體設定不同就標。
 * - `runtime_fast` 是 `null`＝未知（只 codex 比）。`runtime_identity` 的空字串是已知的本機預設帳號，`null` 才是未知。
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

/**
 * 查額度要用哪個身分（issue #541）：額度記在 run **實際起來**的那個身分上（API.md 的 `runtime_identity`：
 * 「改了 `bot.identity` 之後、重啟之前兩者不同；額度一律記在這個身分上」）。所以 daemon 記得的話以它為準，
 * `undefined`（收編的 pane、沒記、沒有 active run）才退回設定值——不然改帳號還沒重啟那段期間，
 * 側欄查的是新帳號的格子，燒的卻是舊帳號的額度。
 */
export function quotaIdentity(bot: { identity: string | null } | null | undefined, run: Run | null): string | null {
  const running = runtimeIdentity(run)
  return running === undefined ? (bot?.identity ?? null) : running
}

/** 模型／強度／fast 這一組是否已知；identity 單獨已知時不能拿 null 猜這三欄。 */
export function runtimeSettingsKnown(run: Run | null): boolean {
  if (!run || (run.state !== 'running' && run.state !== 'starting')) return false
  return run.runtime_model !== null || run.runtime_effort !== null || run.runtime_fast !== null
}

/**
 * 正在跑的 fast：CLI 狀態列回報 → `run.runtime_fast` → `bots.fast`（設定）。model 標籤的 tooltip 與側欄 logo 上的
 * 閃電共用這一份，兩邊才不會一個說 fast、另一個說沒有（設定改了還沒重啟時，設定值跟實際在跑的不同）。
 */
export function runningFast(bot: Bot, run: Run | null): boolean {
  const reported = run?.status?.fast_mode
  if (reported != null) return Boolean(reported)
  return Boolean(runtimeSettingsKnown(run) ? (run!.runtime_fast ?? bot.fast) : bot.fast)
}

/** runtime 欄位全未知＝收編 pane：別拿 `bots` 設定假裝成 runtime。 */
export function runtimeKnown(run: Run | null): boolean {
  return runtimeSettingsKnown(run) || runtimeIdentity(run) !== undefined
}

/**
 * 模型不分大小寫比：codex 0.157 的狀態列印顯示名（`GPT-6-Luna`），daemon 讀回的 runtime 在修正前原樣記下，
 * 跟設定的 id `gpt-6-luna` 逐字比就多出一行假的「模型」差異，fast 也因此不算「只有 fast」而走重啟（#712）。
 */
function sameModel(running: string | null | undefined, configured: string | null | undefined): boolean {
  return (running ?? '').toLowerCase() === (configured ?? '').toLowerCase()
}

/**
 * `runtime_effort` 三態（SPEC §4.4a）：`null`＝未知（沒讀到／收編），單欄不比；`'default'`＝畫面明確寫 CLI 預設，
 * 跟設定 `null` 同義；其餘是具體等級。
 */
export const CLI_DEFAULT_EFFORT = 'default'

/** 具體等級才有意義；`'default'` 正規化成 `null`（CLI 預設），未知另外判斷。 */
function runningEffort(value: string | null): string | null {
  return value === CLI_DEFAULT_EFFORT ? null : value
}

export function runtimeDrift(bot: Bot | null, run: Run | null): RuntimeDriftField[] {
  if (!bot || !run || !runtimeKnown(run)) return []
  const out: RuntimeDriftField[] = []
  // 設定是 null（CLI 預設）又沒有待重啟：runtime 記的是 CLI 實際挑的值（statusLine／狀態列讀回，ModelTag 要用），
  // 重啟也還是 CLI 預設，不是落差（#940）。有值→清成 null 會讓 `needs_restart` 為真，那一筆照列。
  const defaultsStay = (configured: string | null | undefined) => (configured ?? null) === null && !bot.needs_restart
  // runtime 那一欄是 null＝沒讀到（不是 CLI 預設）：不拿設定去猜，其他真的落差照列。
  if (run.runtime_model !== null && !defaultsStay(bot.model) && !sameModel(run.runtime_model, bot.model)) {
    out.push({
      field: 'model',
      label: '模型',
      running: run.runtime_model ?? NONE,
      configured: bot.model ?? NONE,
    })
  }
  const effort = runningEffort(run.runtime_effort)
  if (run.runtime_effort !== null && !defaultsStay(bot.effort) && effort !== (bot.effort ?? null)) {
    out.push({
      field: 'effort',
      label: '強度',
      running: effort ? effortLabel(effort) : NONE,
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

/**
 * 落差只有 codex 的 fast：TUI 的 `/fast` 一個鍵就能切、不必重啟（#393），徽章改走「當場套用」。
 * 混著模型／強度／帳號的落差照舊走重啟。
 */
export function isFastOnlyDrift(kind: string, drift: RuntimeDriftField[]): boolean {
  return kind === 'codex' && drift.length > 0 && drift.every((d) => d.field === 'fast')
}
