/**
 * 「被登出就主動提示我登入」（#838 補充，使用者 2026-10-04：「應該要偵測到有被登出就提示我登入，而不是去身份裡面點」）。
 * 哪些 claude 身分現在該提示、關掉的狀態怎麼比。全是純函式；顯示在 `components/LoginPromptBanner.tsx`。
 *
 * 一個身分要提示，需要同時：① 在那台主機上要重新登入——探測說 `logged_in === false`，或 daemon 記了 `login_needed`
 * （綁著它的 bot 回合授權失敗；遠端 claude 的探測問不出「未登入」，m4p 的 cc1 只有這條）；② 有 bot 綁著它（同一台主機）；
 * ③ 那台主機連得上（連不上時登入也開不了 pane，離線警示條已經在講）。
 */
import type { Bot, Host, IdentityStatusMap, Project } from '../api/types'

export interface PendingLogin {
  host: string
  identity: string
  /** 這一次登出的識別：daemon 的 `login_needed.since`；只有探測說未登入時用固定字 `probe`。 */
  episode: string
  via: 'probe' | 'turn'
  /** 綁著這個身分、在這台主機的 bot。 */
  bots: { id: string; name: string }[]
}

export interface LoginPromptInput {
  bots: Pick<Bot, 'id' | 'name' | 'project_id' | 'identity' | 'kind'>[]
  projects: Pick<Project, 'id' | 'host'>[]
  hosts: Pick<Host, 'name' | 'connected' | 'identity_status'>[]
  localIdentityStatus: IdentityStatusMap
  /** 本機（daemon 與 herdr）連著。 */
  localConnected: boolean
}

export const dismissKey = (host: string, identity: string) => `${host}/${identity}`

export function pendingLogins(s: LoginPromptInput): PendingLogin[] {
  const hostOf = new Map(s.projects.map((p) => [p.id, p.host || 'local']))
  const sources: { host: string; status: IdentityStatusMap }[] = [
    ...(s.localConnected ? [{ host: 'local', status: s.localIdentityStatus }] : []),
    ...s.hosts.filter((h) => h.connected).map((h) => ({ host: h.name, status: h.identity_status })),
  ]
  const out: PendingLogin[] = []
  for (const { host, status } of sources) {
    for (const [identity, st] of Object.entries(status)) {
      if (st.kind !== 'claude') continue
      const turn = st.login_needed ?? null
      if (st.logged_in !== false && !turn) continue
      const bots = s.bots
        .filter((b) => b.kind === 'claude' && b.identity === identity && (hostOf.get(b.project_id) ?? 'local') === host)
        .map((b) => ({ id: b.id, name: b.name }))
      if (bots.length === 0) continue
      out.push({ host, identity, episode: turn?.since ?? 'probe', via: turn ? 'turn' : 'probe', bots })
    }
  }
  return out
}

/** 關掉過的是同一次登出就不再顯示；換了一次（`since` 變了）或之前登入成功過（帳被 [`pruneDismissed`] 清掉）就會再提示。 */
export function visibleLogins(pending: PendingLogin[], dismissed: Record<string, string>): PendingLogin[] {
  return pending.filter((p) => dismissed[dismissKey(p.host, p.identity)] !== p.episode)
}

/** 已經不需要登入的身分，把它關掉過的記錄清掉：下一次又被登出時才會重新提示。 */
export function pruneDismissed(dismissed: Record<string, string>, pending: PendingLogin[]): Record<string, string> {
  const live = new Set(pending.map((p) => dismissKey(p.host, p.identity)))
  const keys = Object.keys(dismissed)
  if (keys.every((k) => live.has(k))) return dismissed
  return Object.fromEntries(keys.filter((k) => live.has(k)).map((k) => [k, dismissed[k]]))
}

/** 給 prune 用的 pending：來源斷線的那幾台，已關掉的 key 原樣保留（斷線≠登入成功）。 */
export function pendingForPrune(
  s: Pick<LoginPromptInput, 'hosts' | 'localConnected'>,
  pending: PendingLogin[],
  dismissed: Record<string, string>,
): PendingLogin[] {
  const live = new Set([...(s.localConnected ? ['local'] : []), ...s.hosts.filter((h) => h.connected).map((h) => h.name)])
  const keep: PendingLogin[] = []
  for (const [k, episode] of Object.entries(dismissed)) {
    const at = k.indexOf('/')
    const host = k.slice(0, at)
    if (at > 0 && !live.has(host)) keep.push({ host, identity: k.slice(at + 1), episode, via: 'probe', bots: [] })
  }
  return keep.length ? [...pending, ...keep] : pending
}

export function loginPromptText(p: PendingLogin): { title: string; detail: string } {
  const where = p.host === 'local' ? '本機' : p.host
  const n = p.bots.length
  return {
    title: `${p.identity}（${where}）已登出`,
    detail:
      p.via === 'turn'
        ? `${n} 顆 bot 在用它，剛才有回合因為登入失效失敗`
        : `偵測到已登出，${n} 顆 bot 在用它`,
  }
}
