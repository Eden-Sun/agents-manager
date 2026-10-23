/**
 * 網址 ↔ store 雙向同步（`docs/goals/routes-2026-09-09.md`；解析在 `lib/routes.ts`）。
 * 網址是 store 的投影：元件照舊走 store，不必知道這個檔。
 */
import { useEffect, useRef } from 'react'
import * as api from '../api'
import { dialogOpen } from '../lib/dialogOpen'
import { popDecision } from '../lib/popGuard'
import { buildRoute, parseRoute, screenKey, type Route } from '../lib/routes'
import { settingsBlocksLeave } from '../lib/settingsLeaveGuard'
import { useStore, type StoreState } from './store'
import { userNavCount } from './userNav'
import { t } from '../lib/i18n'

/** 預覽分頁只給頂層 bot（issue #253）；`ChatPanel` 與網址還原共用這條。 */
export function isTopLevelBot(b: { parent_bot_id: string | null; managed_by: string } | undefined): boolean {
  return Boolean(b) && b!.parent_bot_id === null && b!.managed_by === 'user'
}

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
      return r.settings ? t('{name} · 設定', { name }) : r.tab === 'terminal' ? t('{name} · 終端', { name }) : r.tab === 'preview' ? t('{name} · 預覽', { name }) : name
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
/** 網址 → store 套用中，不回寫。使用者在途中改了選取時由訂閱清掉。 */
let applying = false
/** 導覽世代：每次套用或使用者導覽都前進，非同步結果回來時對不上就丟棄（#1808）。 */
let navGen = 0
/** 路由正在寫 store（見 `write`）。 */
let writing = false
/** 上一次看到的使用者導覽計數（見 `store/userNav.ts`）。 */
let seenNav = 0
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

/** 路由自己寫 store 的那段：訂閱看到時不當成使用者導覽，不會把自己的套用作廢。 */
function write(fn: () => void) {
  writing = true
  try {
    fn()
  } finally {
    writing = false
    seenNav = userNavCount()
  }
}

function backHome(why: string) {
  const s = useStore.getState()
  s.notify('info', t('{why}，已回到首頁。', { why }))
  write(() => s.selectBot(null))
}

/**
 * 網址 → store。連結會過期，找不到就回首頁並說一聲，別靜靜停在空畫面。
 * `gen` 是這一輪的導覽世代：await 回來若已經不是它（使用者又導覽了、或有新的套用），結果一律丟掉（#1808）。
 */
async function applyRoute(r: Route, gen: number) {
  const s = useStore.getState()
  const current = () => gen === navGen
  switch (r.kind) {
    case 'home':
      write(() => s.selectBot(null))
      return
    case 'bot': {
      if (!s.bots.some((b) => b.id === r.botId)) return backHome(t('這個 Bot 已經不在了'))
      if (r.settings) {
        write(() => s.openSettings(r.botId))
        return
      }
      write(() => {
        s.selectBot(r.botId)
        if (r.tab === 'terminal') s.setRightTab('terminal')
        else if (r.tab === 'preview' && isTopLevelBot(s.bots.find((b) => b.id === r.botId))) s.setRightTab('preview')
      })
      return
    }
    case 'project':
      if (!s.projects.some((p) => p.id === r.projectId)) return backHome(t('這個專案已經不在了'))
      write(() => s.selectProject(r.projectId))
      return
    case 'shell': {
      try {
        // pane 活不過 daemon 重啟，要對現況；順便補回網址裡沒有的 `cwd`。
        const shell = (await api.fetchHostShells(r.host)).find((x) => x.pane_id === r.paneId)
        if (!current()) return
        if (shell) {
          write(() => s.viewHostShell(shell))
          return
        }
        // 選單點進去的 pane（§6.5e）不在「自己開的」那份裡；它記在 daemon 的 pane 表，活得過重啟。
        const traced = (await api.fetchAllPanes()).find((x) => x.host === r.host && x.pane_id === r.paneId)
        if (!current()) return
        if (!traced) return backHome(t('這個 shell 已經關掉了'))
        write(() => s.viewPane(traced))
      } catch {
        if (current()) backHome(t('讀不到這台主機的 shell'))
      }
      return
    }
  }
}

/** 套用一條路由；結束後再對一次帳（例如回了首頁，網址要跟著變）。新的套用或使用者導覽會讓舊的結果作廢。 */
function run(r: Route) {
  const gen = ++navGen
  applying = true
  void applyRoute(r, gen).finally(() => {
    if (gen !== navGen) return
    applying = false
    syncNow('replace')
  })
}

/** 最上層的對話框元素（DOM 順序最後一個）：`Modal`／`ConfirmDialog` 以 `e.target` 判斷 Esc 的所有權，所以要派給它本身。 */
function topDialog(): Element | null {
  const all = [...document.querySelectorAll('.modal-backdrop .modal, .confirm-backdrop .confirm, .lightbox, [aria-modal="true"]')].filter(
    (el) => !el.matches('aside.sidebar'),
  )
  return all[all.length - 1] ?? null
}

function onPop() {
  const r = parseRoute(location.pathname)
  const leavingSettings =
    lastRoute.kind === 'bot' && lastRoute.settings && !(r.kind === 'bot' && r.settings && r.botId === lastRoute.botId)
  // 先依畫面上疊了什麼決定（`lib/popGuard.ts`）：對話框 > 抽屜 > 設定未儲存守門 > 導覽。
  switch (popDecision({ drawerOpen, dialogOpen: dialogOpen(), leavingSettings, guardBlocks: settingsBlocksLeave })) {
    case 'close-dialog': {
      // 這一格歷史是借來的：放回去（抽屜開著就還是抽屜那一格），上一頁只關最上層的對話框，其餘都不動（#935）。
      if (drawerOpen) history.pushState({ am: 'drawer' } satisfies HistoryMark, '', here())
      else history.pushState({ am: 'route' } satisfies HistoryMark, '', buildRoute(lastRoute))
      topDialog()?.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }))
      return
    }
    case 'close-drawer':
      // 抽屜開著時的上一頁只關抽屜（那格 URL 沒變）。
      closeDrawer?.()
      return
    case 'stay':
      // 設定有未儲存變更（確認框已由守門打開）：把設定那一格放回去，不套用新路由（#936）。
      history.pushState({ am: 'route' } satisfies HistoryMark, '', buildRoute(lastRoute))
      return
    case 'navigate':
      lastRoute = r
      run(r)
  }
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
  seenNav = userNavCount()
  window.addEventListener('popstate', onPop)

  let pending: Route | null = initial
  useStore.subscribe((s) => {
    // 使用者導覽（點 bot、切分頁、關 shell…）才作廢還沒回來的套用，網址跟著新的選取走。
    // 還原／刷新直接寫選取不算：開機深連結在途時 localStorage 還原不會蓋掉它（#1808）。
    const nav = userNavCount()
    if (nav !== seenNav) {
      seenNav = nav
      if (!writing) {
        navGen++
        applying = false
      }
    }
    if (applying) return
    if (pending) {
      // 清單到齊才判斷得出網址指的東西還在不在。
      if (!s.ready) return
      const r = pending
      pending = null
      // `/` 就尊重 localStorage 還原的選取，由 `syncNow` 寫成網址。
      if (r.kind !== 'home') {
        const gen = ++navGen
        applying = true
        void applyRoute(r, gen).finally(() => {
          booted = true
          if (gen !== navGen) return
          applying = false
          syncNow()
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
