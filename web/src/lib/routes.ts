/**
 * 每個畫面自己的 URL（goal `docs/goals/routes-2026-09-09.md`）。
 *
 * 刻意**不引 router 套件**：路徑只有六種形狀，`parse` / `build` 一對純函式就夠，
 * 而且純函式才測得起來（`routes.test.ts`）。真正跟 store 對接的雙向同步在
 * `store/routeSync.ts`，這個檔不碰 `window`、不碰 store。
 */

export type RouteTab = 'chat' | 'terminal'

export type Route =
  /** 什麼都沒選（`/`）。開機時它代表「網址沒指定」，不是「請清空選取」——見 routeSync。 */
  | { kind: 'home' }
  | { kind: 'bot'; botId: string; tab: RouteTab; settings: boolean }
  | { kind: 'project'; projectId: string }
  | { kind: 'team'; teamId: string }
  | { kind: 'team-new'; projectId: string; issueNumber: number }
  | { kind: 'shell'; host: string; paneId: string }

export const HOME: Route = { kind: 'home' }

/** 壞掉的 `%` 逸出不該讓整個 UI 崩掉，解不開就照原樣當 id（反正之後會查不到而回首頁）。 */
function decodeSeg(s: string): string {
  try {
    return decodeURIComponent(s)
  } catch {
    return s
  }
}

/**
 * `pathname` → Route。任何認不得的路徑一律回首頁：網址是使用者能手打的東西，
 * 錯字不該變成一個半死的畫面。
 */
export function parseRoute(pathname: string, search = ''): Route {
  const p = pathname.split('/').filter(Boolean).map(decodeSeg)
  if (p.length === 0) return HOME

  if (p[0] === 'bots' && p[1]) {
    if (p.length === 2) return { kind: 'bot', botId: p[1], tab: 'chat', settings: false }
    if (p.length === 3 && p[2] === 'terminal') return { kind: 'bot', botId: p[1], tab: 'terminal', settings: false }
    if (p.length === 3 && p[2] === 'settings') return { kind: 'bot', botId: p[1], tab: 'chat', settings: true }
    return HOME
  }

  if (p[0] === 'projects' && p[1]) {
    if (p.length === 2) return { kind: 'project', projectId: p[1] }
    if (p.length === 4 && p[2] === 'teams' && p[3] === 'new') {
      const n = Number(new URLSearchParams(search).get('issue'))
      // 組隊面板一定要有一個 issue 才畫得出來；沒有就退回專案本身，而不是首頁——
      // 使用者要去的地方至少對了一半。
      return Number.isInteger(n) && n > 0
        ? { kind: 'team-new', projectId: p[1], issueNumber: n }
        : { kind: 'project', projectId: p[1] }
    }
    return HOME
  }

  if (p[0] === 'teams' && p[1] && p.length === 2) return { kind: 'team', teamId: p[1] }

  if (p[0] === 'hosts' && p[1] && p[2] === 'shells' && p[3] && p.length === 4) {
    return { kind: 'shell', host: p[1], paneId: p[3] }
  }

  return HOME
}

/** Route → `pathname` + `search`（不含 token：分享出去的連結不該帶憑證）。 */
export function buildRoute(r: Route): string {
  const e = encodeURIComponent
  switch (r.kind) {
    case 'home':
      return '/'
    case 'bot':
      // 設定是蓋在對話上的浮窗，所以它跟 `terminal` 不會同時成立（`openSettings` 會切回對話）。
      return `/bots/${e(r.botId)}${r.settings ? '/settings' : r.tab === 'terminal' ? '/terminal' : ''}`
    case 'project':
      return `/projects/${e(r.projectId)}`
    case 'team':
      return `/teams/${e(r.teamId)}`
    case 'team-new':
      return `/projects/${e(r.projectId)}/teams/new?issue=${r.issueNumber}`
    case 'shell':
      return `/hosts/${e(r.host)}/shells/${e(r.paneId)}`
  }
}

/**
 * 「同一個畫面」的識別碼：一樣就用 `replaceState`，上一頁不該為了對話↔終端多一格。
 * 設定浮窗刻意算成另一個畫面——它開著時 push、關掉時就是上一頁（goal 的要求）。
 */
export function screenKey(r: Route): string {
  switch (r.kind) {
    case 'home':
      return 'home'
    case 'bot':
      return `bot:${r.botId}${r.settings ? ':settings' : ''}`
    case 'project':
      return `project:${r.projectId}`
    case 'team':
      return `team:${r.teamId}`
    case 'team-new':
      return `team-new:${r.projectId}:${r.issueNumber}`
    case 'shell':
      return `shell:${r.host}:${r.paneId}`
  }
}
