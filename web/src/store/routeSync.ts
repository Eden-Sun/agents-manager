/**
 * 網址 ↔ store 雙向同步（`docs/goals/routes-2026-09-09.md`；解析在 `lib/routes.ts`）。
 * 網址是 store 的投影：元件照舊走 store，不必知道這個檔。
 */
import { useEffect, useRef } from 'react'
import * as api from '../api'
import { buildRoute, parseRoute, screenKey, type Route } from '../lib/routes'
import { useStore, type StoreState } from './store'

/** 判斷順序照 `App.tsx` 的 render 分支（誰蓋在誰上面）。 */
export function routeOf(s: StoreState): Route {
  if (s.shellView) return { kind: 'shell', host: s.shellView.host, paneId: s.shellView.paneId }
  if (s.selectedProjectId) return { kind: 'project', projectId: s.selectedProjectId }
  if (s.selectedBotId) {
    const settings = s.settingsBotId === s.selectedBotId
    return { kind: 'bot', botId: s.selectedBotId, tab: settings ? 'chat' : s.rightTab, settings }
  }
  return { kind: 'home' }
}

/** 分頁標題的畫面段；空字串 = 首頁。 */
export function screenTitle(s: StoreState): string {
  const r = routeOf(s)
  switch (r.kind) {
    case 'home':
      return ''
    case 'bot': {
      const name = s.bots.find((b) => b.id === r.botId)?.name
      if (!name) return ''
      return r.settings ? `${name} · 設定` : r.tab === 'terminal' ? `${name} · 終端` : name
    }
    case 'project':
      return s.projects.find((p) => p.id === r.projectId)?.label ?? ''
    case 'shell':
      return `${r.host} · shell`
  }
}

/** `history.state.am`：`drawer` 是手機抽屜借用的那一格。 */
type HistoryMark = { am: 'route' | 'drawer' }

function markOf(): HistoryMark['am'] | null {
  const st: unknown = history.state
  if (st && typeof st === 'object' && 'am' in st) {
    const v = (st as { am: unknown }).am
    if (v === 'route' || v === 'drawer') return v
  }
  return null
}

const here = () => location.pathname + location.search

let started = false
/** 初始路由套用完前一律 replace：開頁不該先留一格空白歷史。 */
let booted = false
/** 網址 → store 套用中，不回寫。 */
let applying = false
let lastRoute: Route = { kind: 'home' }
let drawerOpen = false
let closeDrawer: (() => void) | null = null

function syncNow(force?: 'replace') {
  const r = routeOf(useStore.getState())
  const url = buildRoute(r)
  if (url === here()) {
    lastRoute = r
    return
  }
  const replace =
    force === 'replace' ||
    !booted ||
    // 抽屜借的格子直接換掉，不再疊一格。
    markOf() === 'drawer' ||
    // 首頁是過場（`refreshState` 會自動選第一個 bot），不留歷史。
    lastRoute.kind === 'home' ||
    screenKey(r) === screenKey(lastRoute)
  lastRoute = r
  const mark: HistoryMark = { am: 'route' }
  if (replace) history.replaceState(mark, '', url)
  else history.pushState(mark, '', url)
}

function backHome(why: string) {
  const s = useStore.getState()
  s.notify('info', `${why}，已回到首頁。`)
  s.selectBot(null)
}

/** 網址 → store。連結會過期，找不到就回首頁並說一聲，別靜靜停在空畫面。 */
async function applyRoute(r: Route) {
  const s = useStore.getState()
  switch (r.kind) {
    case 'home':
      s.selectBot(null)
      return
    case 'bot': {
      if (!s.bots.some((b) => b.id === r.botId)) return backHome('這個 Bot 已經不在了')
      if (r.settings) {
        s.openSettings(r.botId)
        return
      }
      s.selectBot(r.botId)
      if (r.tab === 'terminal') s.setRightTab('terminal')
      return
    }
    case 'project':
      if (!s.projects.some((p) => p.id === r.projectId)) return backHome('這個專案已經不在了')
      s.selectProject(r.projectId)
      return
    case 'shell': {
      try {
        // pane 活不過 daemon 重啟，要對現況；順便補回網址裡沒有的 `cwd`。
        const shell = (await api.fetchHostShells(r.host)).find((x) => x.pane_id === r.paneId)
        if (shell) return s.viewHostShell(shell)
        // 選單點進去的 pane（§6.5e）不在「自己開的」那份裡；它記在 daemon 的 pane 表，活得過重啟。
        const traced = (await api.fetchAllPanes()).find((x) => x.host === r.host && x.pane_id === r.paneId)
        if (!traced) return backHome('這個 shell 已經關掉了')
        s.viewPane(traced)
      } catch {
        backHome('讀不到這台主機的 shell')
      }
      return
    }
  }
}

/** 結束後再對一次帳（例如回了首頁，網址要跟著變）。 */
function run(r: Route) {
  applying = true
  void applyRoute(r).finally(() => {
    applying = false
    syncNow('replace')
  })
}

function onPop() {
  // 抽屜開著時的上一頁只關抽屜（那格 URL 沒變）。
  if (drawerOpen) {
    closeDrawer?.()
    return
  }
  const r = parseRoute(location.pathname)
  lastRoute = r
  run(r)
}

/**
 * 從 `main.tsx` 叫一次。`?token=` 只在首次載入用，第一次 `replaceState` 會把它從網址拿掉——
 * 分享出去的連結不該帶憑證。
 */
export function startRouteSync() {
  if (started || typeof window === 'undefined') return
  started = true
  const initial = parseRoute(location.pathname)
  lastRoute = initial
  window.addEventListener('popstate', onPop)

  let pending: Route | null = initial
  useStore.subscribe((s) => {
    if (applying) return
    if (pending) {
      // 清單到齊才判斷得出網址指的東西還在不在。
      if (!s.ready) return
      const r = pending
      pending = null
      // `/` 就尊重 localStorage 還原的選取，由 `syncNow` 寫成網址。
      if (r.kind !== 'home') {
        applying = true
        void applyRoute(r).finally(() => {
          applying = false
          syncNow()
          booted = true
        })
        return
      }
      syncNow()
      booted = true
      return
    }
    syncNow()
  })
}

/**
 * 手機抽屜開時借一格歷史（URL 不變、`history.state` 記號），上一頁＝關抽屜。
 * 抽屜內導覽不必特別處理：store 訂閱早於 re-render，`syncNow` 會 replace 掉這格。
 */
export function useDrawerRoute(open: boolean, close: () => void) {
  const closeRef = useRef(close)
  useEffect(() => {
    closeRef.current = close
  })
  useEffect(() => {
    drawerOpen = open
    closeDrawer = () => closeRef.current()
    if (open) {
      if (markOf() !== 'drawer') {
        const mark: HistoryMark = { am: 'drawer' }
        history.pushState(mark, '', here())
      }
    } else if (markOf() === 'drawer') {
      // ✕／scrim／Esc 關的：還回借來的格子。延後一個 tick（2026-09-09 手機點 bot 切不過去）：
      // capture 階段先關抽屜、bubble 的 selectBot 才 replace 新路由，同步 back() 會被拉回舊 bot。
      const t = setTimeout(() => {
        if (markOf() === 'drawer') history.back()
      }, 0)
      return () => clearTimeout(t)
    }
    return () => {
      drawerOpen = false
      closeDrawer = null
    }
  }, [open])
}
