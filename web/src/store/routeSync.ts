/**
 * 網址 ↔ store 的雙向同步（goal `docs/goals/routes-2026-09-09.md`）。
 *
 * 純函式的路徑解析在 `lib/routes.ts`；這裡只做三件事：
 *
 * 1. store 的選取變了 → 寫進 `history`（同一個畫面內的切換用 `replaceState`）。
 * 2. `popstate`（上一頁／下一頁）→ 呼叫對應的 select。
 * 3. 開頁時把 `location.pathname` 套用到 store——但要等 `ready`，因為「這個 bot 還在不在」
 *    得先有 `GET /api/state` 的清單才答得出來。
 *
 * 元件不必知道這個檔存在：側欄與所有「開啟 X」的按鈕維持原本的 onClick（走 store），
 * 網址是 store 的投影，不是反過來。
 */
import { useEffect, useRef } from 'react'
import * as api from '../api'
import { buildRoute, parseRoute, screenKey, type Route } from '../lib/routes'
import { useStore, type StoreState } from './store'

/** 目前畫面上真的是哪一個 Route。順序照 `App.tsx` 的 render 分支（誰蓋在誰上面）。 */
export function routeOf(s: StoreState): Route {
  // shell 非 null 時，不管它是整個主面板還是 ChatPanel 的第三個分頁，畫面上就是那個終端。
  if (s.shellView) return { kind: 'shell', host: s.shellView.host, paneId: s.shellView.paneId }
  if (s.teamLaunch) return { kind: 'team-new', projectId: s.teamLaunch.projectId, issueNumber: s.teamLaunch.issueNumber }
  if (s.selectedTeamId) return { kind: 'team', teamId: s.selectedTeamId }
  if (s.selectedProjectId) return { kind: 'project', projectId: s.selectedProjectId }
  if (s.selectedBotId) {
    const settings = s.settingsBotId === s.selectedBotId
    return { kind: 'bot', botId: s.selectedBotId, tab: settings ? 'chat' : s.rightTab, settings }
  }
  return { kind: 'home' }
}

/** 分頁標題裡「畫面」那一段（空字串 = 首頁，只留 app 名稱）。 */
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
    case 'team': {
      const t = s.teams[r.teamId]
      return t ? `#${t.issue_number} ${t.issue_title} · Team` : 'Team'
    }
    case 'team-new':
      return `#${r.issueNumber} · 組隊`
    case 'shell':
      return `${r.host} · shell`
  }
}

// ---------------------------------------------------------------- history

/** `history.state.am`：`route` 是我們寫的畫面，`drawer` 是手機抽屜借用的那一格。 */
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
/** 初始路由套用完之前，所有寫入都用 `replaceState`（開頁不該先留一格空白歷史）。 */
let booted = false
/** 正在把網址套用到 store：這段期間 store 的變動是「已經反映在網址上」的，不要回寫。 */
let applying = false
let lastRoute: Route = { kind: 'home' }
let drawerOpen = false
let closeDrawer: (() => void) | null = null

/** store → 網址。回傳有沒有真的寫。 */
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
    // 抽屜借的那一格 URL 跟它下面那格一樣：導覽時把它換掉，而不是再疊一格。
    markOf() === 'drawer' ||
    // 首頁是過場（`refreshState` 沒選取時會自動選第一個 bot），不留在歷史裡。
    lastRoute.kind === 'home' ||
    screenKey(r) === screenKey(lastRoute)
  lastRoute = r
  const mark: HistoryMark = { am: 'route' }
  if (replace) history.replaceState(mark, '', url)
  else history.pushState(mark, '', url)
}

// ---------------------------------------------------------------- 套用

function backHome(why: string) {
  const s = useStore.getState()
  s.notify('info', `${why}，已回到首頁。`)
  s.selectBot(null)
}

/**
 * 網址 → store。找不到對應的東西就回首頁並說一聲——連結會過期（bot 被刪、shell 被關），
 * 靜靜停在一個空畫面比說出來更難懂。
 */
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
    case 'team':
      if (!s.teams[r.teamId]) return backHome('這個 Team 已經不在了')
      s.selectTeam(r.teamId)
      return
    case 'team-new': {
      const p = s.projects.find((x) => x.id === r.projectId)
      if (!p) return backHome('這個專案已經不在了')
      // repo 不進網址：一個 issue 編號在專案裡就是唯一的，而網址不必背 submodule 的名字。
      // 從連結進來一律當專案本身的 repo；submodule 的組隊還是從 Issues 列表點進去。
      s.openTeamLaunch(r.projectId, r.issueNumber, p.github?.repo ?? '')
      return
    }
    case 'shell': {
      if (!s.hostShellSupported) return backHome('這個 daemon 沒有主機 shell')
      try {
        // pane 活不過 daemon 重啟，所以連結一定要對一次現況，順便把 `cwd`（網址裡沒有）補回來。
        const shell = (await api.fetchHostShells(r.host)).find((x) => x.pane_id === r.paneId)
        if (!shell) return backHome('這個 shell 已經關掉了')
        s.viewHostShell(shell)
      } catch {
        backHome('讀不到這台主機的 shell')
      }
      return
    }
  }
}

/** 套用期間擋住回寫，結束後再對一次帳（例如回了首頁，網址要跟著變）。 */
function run(r: Route) {
  applying = true
  void applyRoute(r).finally(() => {
    applying = false
    syncNow('replace')
  })
}

function onPop() {
  // 抽屜開著時的上一頁：先關抽屜、不切畫面（那一格的 URL 跟下面那格一樣，路由本來就沒變）。
  if (drawerOpen) {
    closeDrawer?.()
    return
  }
  const r = parseRoute(location.pathname, location.search)
  lastRoute = r
  run(r)
}

/**
 * 從 `main.tsx` 叫一次。`?token=` 只在第一次載入用（transport 拿到 `GET /api/session`
 * 之後就自己快取了），套用路由時的第一次 `replaceState` 會順手把它從網址上拿掉——
 * 分享出去的連結不該帶憑證。
 */
export function startRouteSync() {
  if (started || typeof window === 'undefined') return
  started = true
  const initial = parseRoute(location.pathname, location.search)
  lastRoute = initial
  window.addEventListener('popstate', onPop)

  let pending: Route | null = initial
  useStore.subscribe((s) => {
    if (applying) return
    if (pending) {
      // bot / project / team 清單要先到齊，才判斷得出網址指的東西還在不在。
      if (!s.ready) return
      const r = pending
      pending = null
      // 網址沒指定（`/`）就尊重 store 從 localStorage 還原的選取，別把它清掉；
      // 底下的 `syncNow` 會把那個選取寫成真正的網址。
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

// ---------------------------------------------------------------- 抽屜

/**
 * 手機抽屜也吃「上一頁」：開的時候借一格歷史（URL 不變，只在 `history.state` 上做記號），
 * 按上一頁就是把它關掉，而不是離開這個畫面。
 *
 * 在抽屜裡點了會換頁的東西時不必特別處理：store 的訂閱比 React 的 re-render 早跑，
 * `syncNow` 會看到自己站在 `drawer` 那一格而改用 `replaceState` 把它換成新畫面。
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
      // 用 ✕ / scrim / Esc 關的：把借來的那一格還回去，歷史才不會愈積愈長。
      //
      // **要延後一個 tick**（2026-09-09 手機點 bot 切不過去）：點側欄的 bot 列時，`App` 的
      // `onClickCapture` 先關抽屜，React 在 capture 階段結束就把這個 effect 跑掉；bubble
      // 階段的 `selectBot` 之後才把新路由 replace 到這一格上。若在這裡同步 `back()`，晚到的
      // popstate 會把畫面拉回上一個 bot。延後再看一次：那一格已經被新路由接手就不用還。
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
