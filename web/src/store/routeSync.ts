import { useEffect, useRef } from 'react'
import * as api from '../api'
import { dialogOpen } from '../lib/dialogOpen'
import { popDecision } from '../lib/popGuard'
import { buildRoute, parseRoute, screenKey, type Route } from '../lib/routes'
import { settingsBlocksLeave } from '../lib/settingsLeaveGuard'
import { useStore, type StoreState } from './store'
import { userNavCount } from './userNav'
import { t } from '../lib/i18n'

export function isTopLevelBot(b: { parent_bot_id: string | null; managed_by: string } | undefined): boolean {
  return Boolean(b) && b!.parent_bot_id === null && b!.managed_by === 'user'
}

export function routeOf(s: StoreState): Route {
  if (s.shellView) return { kind: 'shell', host: s.shellView.host, paneId: s.shellView.paneId }
  if (s.selectedProjectId) return { kind: 'project', projectId: s.selectedProjectId }
  if (s.selectedBotId) {
    const settings = s.settingsBotId === s.selectedBotId
    return { kind: 'bot', botId: s.selectedBotId, tab: settings ? 'chat' : s.rightTab, settings }
  }
  return { kind: 'home' }
}

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
let booted = false
let applying = false
let navGen = 0
let writing = false
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
    markOf() === 'drawer' ||
    lastRoute.kind === 'home' ||
    screenKey(r) === screenKey(lastRoute)
  lastRoute = r
  const mark: HistoryMark = { am: 'route' }
  if (replace) history.replaceState(mark, '', url)
  else history.pushState(mark, '', url)
}

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
        const shell = (await api.fetchHostShells(r.host)).find((x) => x.pane_id === r.paneId)
        if (!current()) return
        if (shell) {
          write(() => s.viewHostShell(shell))
          return
        }
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

function run(r: Route) {
  const gen = ++navGen
  applying = true
  void applyRoute(r, gen).finally(() => {
    if (gen !== navGen) return
    applying = false
    syncNow('replace')
  })
}

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
  switch (popDecision({ drawerOpen, dialogOpen: dialogOpen(), leavingSettings, guardBlocks: settingsBlocksLeave })) {
    case 'close-dialog': {
      if (drawerOpen) history.pushState({ am: 'drawer' } satisfies HistoryMark, '', here())
      else history.pushState({ am: 'route' } satisfies HistoryMark, '', buildRoute(lastRoute))
      topDialog()?.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }))
      return
    }
    case 'close-drawer':
      closeDrawer?.()
      return
    case 'stay':
      history.pushState({ am: 'route' } satisfies HistoryMark, '', buildRoute(lastRoute))
      return
    case 'navigate':
      lastRoute = r
      run(r)
  }
}

export function startRouteSync(): () => void {
  if (started || typeof window === 'undefined') return () => {}
  started = true
  const initial = parseRoute(location.pathname)
  lastRoute = initial
  seenNav = userNavCount()
  window.addEventListener('popstate', onPop)

  let pending: Route | null = initial
  const unsub = useStore.subscribe((s) => {
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
      if (!s.ready) return
      const r = pending
      pending = null
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

  return () => {
    window.removeEventListener('popstate', onPop)
    unsub()
    started = false
  }
}

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
