// `.ts` 副檔名是為了 `node --test --experimental-strip-types` 跑得起來（同
// `components/teamPanelLogic.ts`）：node 的 resolver 不會自己補副檔名。
import { effortLabel } from '../api/types.ts'
import type { Bot, Run } from '../api/types.ts'

/**
 * 「設定」與「實際在跑的」不一致（SPEC §4.4a）。
 *
 * 2026-09-09 的實況：一顆 codex 成員在 AG Man 上寫 `gpt-5.6-luna-High`，同一顆 bot 的終端底部
 * codex 自己印的是 `gpt-5.6-luna xhigh fast`。原因不是旗標沒送到——process 的 argv 上就是
 * `-c model_reasoning_effort="xhigh"`——而是**改設定的時候 codex 沒有重啟**：codex 的模型與
 * 強度只有啟動時吃得到（claude / grok 的 TUI 有 `/model`、`/effort`，daemon 會直接送進去，
 * codex 沒有對應的 slash 指令），而 UI 只顯示自己資料庫裡那份「下次啟動會用的設定」。
 *
 * daemon 現在把每個 run 真正啟動時用的 argv 讀回來存在 `run.runtime_*`，這裡就是拿它跟
 * `bot.*` 比。`runtime_*` 是 `null` 代表 daemon 沒有親手啟動它（收編的 pane），這時什麼都不
 * 比——不知道就別猜。
 */
export interface RuntimeDriftField {
  field: 'model' | 'effort' | 'fast'
  label: string
  /** 現在跑的那個值（已翻成人看的字）。 */
  running: string
  /** 設定成什麼、重啟後會變成的值。 */
  configured: string
}

/** `fast` 只有 codex 會變成啟動旗標（`-c service_tier="priority"`），其他 kind 存了也不會送。 */
function comparesFast(kind: string): boolean {
  return kind === 'codex'
}

const NONE = '（CLI 預設）'

/**
 * daemon 知不知道這個 run 實際在跑什麼。三個欄位都是 `null` = 舊 daemon、或收編來的 pane
 * （argv 不是我們組的）：不知道就不要拿 `bots` 的設定假裝成 runtime。
 */
export function runtimeKnown(run: Run | null): boolean {
  if (!run) return false
  if (run.state !== 'running' && run.state !== 'starting') return false
  return run.runtime_model !== null || run.runtime_effort !== null || run.runtime_fast !== null
}

export function runtimeDrift(bot: Bot | null, run: Run | null): RuntimeDriftField[] {
  if (!bot || !run || !runtimeKnown(run)) return []
  const out: RuntimeDriftField[] = []
  if ((run.runtime_model ?? null) !== (bot.model ?? null)) {
    out.push({
      field: 'model',
      label: '模型',
      running: run.runtime_model ?? NONE,
      configured: bot.model ?? NONE,
    })
  }
  if ((run.runtime_effort ?? null) !== (bot.effort ?? null)) {
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
  return out
}

/** 「強度 實際 xhigh、設定 High」——badge 與 tooltip 共用的一行。 */
export function driftLine(d: RuntimeDriftField): string {
  return `${d.label} 實際 ${d.running}、設定 ${d.configured}`
}

/** tooltip 全文：每個不一致的欄位一行，最後一行說怎麼辦。 */
export function driftTitle(drift: RuntimeDriftField[]): string {
  if (!drift.length) return ''
  return `${drift.map(driftLine).join('\n')}\n重啟這顆 bot 才會換成設定的值。`
}
