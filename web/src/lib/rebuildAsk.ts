/**
 * 「請 AGM 現在重建」這顆按鈕的送出與結果（RebuildBadge）。
 *
 * 以前：每按一次換一個 `ui-rebuild-now-${Date.now()}`（回應斷在路上時重按就是兩則 prompt）、不看 `delivery`
 * （`failed`／`unknown` 也說「已請 AGM 開始重建」）、AGM 回合進行中必定 409 卻只說「請再試一次」
 * ——這正是重建申請在排隊時最常見的狀態（第二輪 review M5）。web 的 prompt 遇到 in-flight 一律 409，
 * daemon 不替人排隊（API.md §5）。
 */
import { ApiError } from '../api/types'

export type RebuildAskOutcome =
  | { kind: 'sent' }
  /** AGM 回合進行中（409 `a turn is already in flight`）：沒送出。 */
  | { kind: 'busy' }
  /** 其他 409（`composer_busy` 之類）：沒送出，同一個 crid 稍後重送即可。 */
  | { kind: 'retry'; reason: string }
  /** 200 但 `delivery:"failed"`：那一回合已經失敗，AGM 沒收到。 */
  | { kind: 'undelivered' }
  /** 200 但 `delivery:"unknown"`：送出去了、不知道到了沒。 */
  | { kind: 'unknown' }
  | { kind: 'error'; message: string }

export function classifyRebuildAsk(result: { delivery: string } | null, error: unknown): RebuildAskOutcome {
  if (result) {
    if (result.delivery === 'failed') return { kind: 'undelivered' }
    if (result.delivery === 'unknown') return { kind: 'unknown' }
    return { kind: 'sent' }
  }
  if (error instanceof ApiError && error.status === 409) {
    const reason = String(error.body.reason ?? error.body.message ?? '')
    if (/in flight/.test(reason)) return { kind: 'busy' }
    return { kind: 'retry', reason }
  }
  return { kind: 'error', message: error instanceof Error ? error.message : String(error) }
}

/**
 * 同一次申請沿用同一個 crid：沒送出（409、網路斷掉、`unknown`）重按都是同一則，daemon 回同一個回合；
 * 確定有結果（送到、或那一回合已經 failed——同一個 crid 只會拿回那個失敗的回合）才換新的。
 */
export function rebuildAsker(newId: () => string) {
  let pending: string | null = null
  return async function ask(request: (crid: string) => Promise<{ delivery: string }>): Promise<RebuildAskOutcome> {
    const crid = pending ?? newId()
    pending = crid
    let out: RebuildAskOutcome
    try {
      out = classifyRebuildAsk(await request(crid), null)
    } catch (e) {
      out = classifyRebuildAsk(null, e)
    }
    if (out.kind === 'sent' || out.kind === 'undelivered') pending = null
    return out
  }
}

/** 使用者看到的那一句。`close`＝可以收起彈窗。 */
export function rebuildAskNotice(o: RebuildAskOutcome): { level: 'info' | 'error'; text: string; close: boolean } {
  switch (o.kind) {
    case 'sent':
      return { level: 'info', text: '已請 AGM 開始重建', close: true }
    case 'busy':
      return { level: 'error', text: 'AGM 正在處理別的事（回合進行中），這則沒送出——等它這一輪結束再按一次。', close: false }
    case 'retry':
      return { level: 'error', text: `AGM 的輸入框暫時送不進去${o.reason ? `（${o.reason}）` : ''}，稍後再按一次。`, close: false }
    case 'undelivered':
      return { level: 'error', text: 'AGM 沒收到（delivery=failed，可能停在確認畫面）；請到 AGM 的對話看一下再試。', close: false }
    case 'unknown':
      return { level: 'error', text: '已送出，但送達狀態不明（delivery=unknown）；請到 AGM 的對話確認，別重複要求。', close: true }
    case 'error':
      return { level: 'error', text: `送給 AGM 失敗：${o.message}，請再試一次`, close: false }
  }
}
