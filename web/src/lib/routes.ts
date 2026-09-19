/**
 * 每個畫面的 URL（goal `docs/goals/routes-2026-09-09.md`）。刻意不引 router：四種形狀，純函式
 * 好測；跟 store 同步在 `store/routeSync.ts`，這裡不碰 `window`／store。
 */

export type RouteTab = 'chat' | 'terminal' | 'preview'

export type Route =
  /** 開機時代表「網址沒指定」，不是「清空選取」——見 routeSync。 */
  | { kind: 'home' }
  | { kind: 'bot'; botId: string; tab: RouteTab; settings: boolean }
  | { kind: 'project'; projectId: string }
  | { kind: 'shell'; host: string; paneId: string }

export const HOME: Route = { kind: 'home' }

/** 壞掉的 `%` 逸出照原樣當 id，別讓 UI 崩掉。 */
function decodeSeg(s: string): string {
  try {
    return decodeURIComponent(s)
  } catch {
    return s
  }
}

/** 認不得的路徑一律回首頁：手打錯字不該變成半死的畫面。 */
export function parseRoute(pathname: string): Route {
  const p = pathname.split('/').filter(Boolean).map(decodeSeg)
  if (p.length === 0) return HOME

  if (p[0] === 'bots' && p[1]) {
    if (p.length === 2) return { kind: 'bot', botId: p[1], tab: 'chat', settings: false }
    if (p.length === 3 && p[2] === 'terminal') return { kind: 'bot', botId: p[1], tab: 'terminal', settings: false }
    if (p.length === 3 && p[2] === 'preview') return { kind: 'bot', botId: p[1], tab: 'preview', settings: false }
    if (p.length === 3 && p[2] === 'settings') return { kind: 'bot', botId: p[1], tab: 'chat', settings: true }
    return HOME
  }

  if (p[0] === 'projects' && p[1]) {
    if (p.length === 2) return { kind: 'project', projectId: p[1] }
    return HOME
  }

  if (p[0] === 'hosts' && p[1] && p[2] === 'shells' && p[3] && p.length === 4) {
    return { kind: 'shell', host: p[1], paneId: p[3] }
  }

  return HOME
}

/** 不含 token：分享出去的連結不該帶憑證。 */
export function buildRoute(r: Route): string {
  const e = encodeURIComponent
  switch (r.kind) {
    case 'home':
      return '/'
    case 'bot':
      // 設定浮窗蓋在對話上，不與 `terminal` 同時成立。
      return `/bots/${e(r.botId)}${r.settings ? '/settings' : r.tab === 'terminal' ? '/terminal' : r.tab === 'preview' ? '/preview' : ''}`
    case 'project':
      return `/projects/${e(r.projectId)}`
    case 'shell':
      return `/hosts/${e(r.host)}/shells/${e(r.paneId)}`
  }
}

/** 一樣就 `replaceState`（對話↔終端不多一格上一頁）；設定浮窗刻意算另一畫面，關掉＝上一頁（goal 要求）。 */
export function screenKey(r: Route): string {
  switch (r.kind) {
    case 'home':
      return 'home'
    case 'bot':
      return `bot:${r.botId}${r.settings ? ':settings' : ''}`
    case 'project':
      return `project:${r.projectId}`
    case 'shell':
      return `shell:${r.host}:${r.paneId}`
  }
}
