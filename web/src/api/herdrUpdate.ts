/**
 * `POST /api/hosts/{name}/herdr-update`（SPEC §6.9 herdr 一鍵更新，完整重啟版）。
 * 回 202 就表示背景開跑；進度走 WS `herdr_update_progress` / `herdr_update_done`，重整後靠 `GET /api/state` 的 `herdr_updates` 對帳。
 */
import { arr, isRec, pick, str } from './normalize'
import { ApiError } from './types'
import { rawTransport } from './index'

export interface HerdrBotRef {
  bot_id: string
  name: string
}

export interface HerdrChildLost extends HerdrBotRef {
  parent_bot_id: string
}

export interface HerdrUpdateStarted {
  update_id: string
  host: string
  target_version: string
  will_resume: HerdrBotRef[]
  children_lost: HerdrChildLost[]
}

export function toBotRefs(v: unknown): HerdrBotRef[] {
  return arr(v).flatMap((x) => (isRec(x) && str(pick(x, 'bot_id')) ? [{ bot_id: str(pick(x, 'bot_id')), name: str(pick(x, 'name')) }] : []))
}

export function toChildrenLost(v: unknown): HerdrChildLost[] {
  return arr(v).flatMap((x) =>
    isRec(x) && str(pick(x, 'bot_id'))
      ? [{ bot_id: str(pick(x, 'bot_id')), name: str(pick(x, 'name')), parent_bot_id: str(pick(x, 'parent_bot_id')) }]
      : [],
  )
}

export async function startHerdrUpdate(host: string, targetVersion: string): Promise<HerdrUpdateStarted> {
  const raw = await rawTransport.request('POST', `/hosts/${encodeURIComponent(host)}/herdr-update`, { target_version: targetVersion })
  const o = isRec(raw) ? raw : {}
  return {
    update_id: str(pick(o, 'update_id')),
    host: str(pick(o, 'host'), host),
    target_version: str(pick(o, 'target_version'), targetVersion),
    will_resume: toBotRefs(pick(o, 'will_resume')),
    children_lost: toChildrenLost(pick(o, 'children_lost')),
  }
}

/** 409／403／404 → 給人看的一句（API.md §12.7b）。daemon 的 409／403 都帶寫好的 `message`，有就用它；這裡只是舊 daemon 或沒帶時的退路。 */
export function herdrStartErrText(e: unknown, host: string): string {
  if (e instanceof ApiError) {
    const human = e.body.message
    if (typeof human === 'string' && human.trim()) return human.trim()
    if (e.status === 404 && !e.body.reason) return `這顆 daemon 還沒有 herdr 一鍵更新（HTTP 404），或不認得主機 ${host}`
    if (e.status === 405) return '這顆 daemon 還沒有 herdr 一鍵更新（HTTP 405），二進位比前端舊；要重建並重啟 daemon'
    switch (e.body.reason) {
      case 'unsupported_host':
        return `${host} 是遠端主機，這版只支援更新本機的 herdr`
      case 'shared_session':
        return `${host} 的 herdr session 跟別的 daemon 共用，不能從這裡重啟`
      case 'stale_target': {
        const cur = e.body.current_target
        return typeof cur === 'string' && cur ? `要升的版本已經變成 ${cur}，關掉框重開一次` : '已經沒有等著升級的新版（可能剛升好了）'
      }
      case 'herdr_update_in_progress':
        return '已經有一次 herdr 更新在跑'
      case 'ui_only':
        return '只能從網頁按，Bot 不能自己觸發 herdr 更新'
    }
  }
  return e instanceof Error ? e.message : String(e)
}
