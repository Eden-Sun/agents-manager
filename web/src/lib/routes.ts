
export type RouteTab = 'chat' | 'terminal' | 'preview'

export type Route =
  | { kind: 'home' }
  | { kind: 'bot'; botId: string; tab: RouteTab; settings: boolean }
  | { kind: 'project'; projectId: string }
  | { kind: 'shell'; host: string; paneId: string }

export const HOME: Route = { kind: 'home' }

function decodeSeg(s: string): string {
  try {
    return decodeURIComponent(s)
  } catch {
    return s
  }
}

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

export function buildRoute(r: Route): string {
  const e = encodeURIComponent
  switch (r.kind) {
    case 'home':
      return '/'
    case 'bot':
      return `/bots/${e(r.botId)}${r.settings ? '/settings' : r.tab === 'terminal' ? '/terminal' : r.tab === 'preview' ? '/preview' : ''}`
    case 'project':
      return `/projects/${e(r.projectId)}`
    case 'shell':
      return `/hosts/${e(r.host)}/shells/${e(r.paneId)}`
  }
}

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
