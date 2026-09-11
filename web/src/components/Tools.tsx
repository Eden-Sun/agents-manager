import { useEffect, useMemo, useRef, useState } from 'react'
import { useShallow } from 'zustand/react/shallow'
import type { BotKind, ToolMap } from '../api/types'
import { BOT_KINDS } from '../api/types'
import { useMenuKeys } from '../hooks/useMenuKeys'
import { missingTools, projectHostName, runningBotsOnHost, toolsOfHost, useStore } from '../store/store'
import { KindTag } from './KindTag'

/**
 * v4.0 agent-CLI detection (`hosts[].tools`) and "install via a running bot"
 * (`POST /api/hosts/:name/tools/install`). The daemon turns that into a prompt for the
 * chosen bot, which installs + logs in inside its own pane (login usually ends up as a
 * `blocked` prompt the user answers in the existing panel).
 */

export function hostLabel(host: string): string {
  return host === 'local' ? '本機' : host
}

/**
 * The install button: picks a running bot on that host (a popover when there are several,
 * straight through when there is one, a hint when there is none).
 */
export function InstallToolButton({ host, kind, small }: { host: string; kind: BotKind; small?: boolean }) {
  const candidates = useStore(useShallow((s) => runningBotsOnHost(s, host)))
  const busy = useStore((s) => Boolean(s.busy[`install:${host}:${kind}`]))
  const installTool = useStore((s) => s.installTool)
  const notify = useStore((s) => s.notify)
  const [open, setOpen] = useState(false)
  const wrap = useRef<HTMLSpanElement>(null)
  const btnRef = useRef<HTMLButtonElement>(null)
  const menuRef = useRef<HTMLUListElement>(null)
  const menuKeys = useMenuKeys(open, menuRef, btnRef, () => setOpen(false))

  useEffect(() => {
    if (!open) return
    const onDoc = (e: MouseEvent) => {
      if (wrap.current && !wrap.current.contains(e.target as Node)) setOpen(false)
    }
    document.addEventListener('mousedown', onDoc)
    return () => document.removeEventListener('mousedown', onDoc)
  }, [open])

  const go = (botId: string) => {
    setOpen(false)
    void installTool(host, kind, botId)
  }

  const click = () => {
    if (candidates.length === 0) {
      notify('error', `${hostLabel(host)} 上沒有執行中的 Bot；請先啟動任一 Bot（任何 kind 都可以）再安裝 ${kind}`)
      return
    }
    if (candidates.length === 1) {
      go(candidates[0].id)
      return
    }
    setOpen(!open)
  }

  return (
    <span className="install" ref={wrap} onClick={(e) => e.stopPropagation()}>
      <button
        ref={btnRef}
        type="button"
        className={`${small ? 'mini-btn' : 'btn'} install-btn`}
        disabled={busy}
        aria-haspopup={candidates.length > 1 ? 'menu' : undefined}
        aria-expanded={open}
        title={`請 ${hostLabel(host)} 上一個執行中的 Bot 安裝並登入 ${kind}（會在它的 pane 執行，登入 URL 出現在 blocked 面板）`}
        onClick={click}
      >
        {busy ? '送出中…' : small ? '安裝' : '用現有 agent 安裝'}
      </button>
      {open ? (
        // 點一下就送出安裝、沒有「選取中」可言——是選單不是 listbox。項目本來沒有 tabIndex，
        // 鍵盤根本選不到；現在打開時焦點進第一項，↑/↓ 移動、Enter/Space 送出、Esc 收回。
        <ul ref={menuRef} className="install-pop" role="menu" tabIndex={-1} aria-label={`選擇執行安裝的 Bot（${hostLabel(host)}）`} onKeyDown={menuKeys}>
          {candidates.map((b) => (
            <li
              key={b.id}
              role="menuitem"
              tabIndex={-1}
              className="mention-item"
              onMouseDown={(e) => e.preventDefault()}
              onClick={() => go(b.id)}
              onKeyDown={(e) => {
                if (e.key !== 'Enter' && e.key !== ' ') return
                e.preventDefault()
                e.stopPropagation()
                go(b.id)
              }}
            >
              <KindTag kind={b.kind} />
              <span>{b.name}</span>
            </li>
          ))}
        </ul>
      ) : null}
    </span>
  )
}

/** Three little badges (✓ installed / ✗ missing / ! not logged in) for a host's tools. */
export function ToolBadges({ host, tools }: { host: string; tools: ToolMap }) {
  return (
    <span className="tool-badges" aria-label={`${hostLabel(host)} 的 agent CLI`}>
      {BOT_KINDS.map((k) => {
        const t = tools[k]
        const state = !t.installed ? 'missing' : t.logged_in === false ? 'nologin' : 'ok'
        const title = !t.installed
          ? `${k}：未安裝`
          : `${k}：已安裝${t.version ? ` ${t.version}` : ''}${t.path ? `（${t.path}）` : ''}${t.logged_in === false ? '，未登入' : t.logged_in ? '，已登入' : ''}`
        return (
          <span key={k} className={`tool-badge ${state}`} title={title}>
            <KindTag kind={k} title={title} />
            <span className="tool-mark" aria-hidden="true">
              {state === 'ok' ? '✓' : state === 'missing' ? '✗' : '!'}
            </span>
            {state !== 'ok' ? <InstallToolButton host={host} kind={k} small /> : null}
          </span>
        )
      })}
    </span>
  )
}

function summarizeMissing(byHost: Map<string, BotKind[]>): string {
  const entries = [...byHost.entries()]
  if (entries.length === 1 && entries[0][1].length === 1) {
    const [host, kinds] = entries[0]
    const kindLabel = kinds[0] === 'claude' ? 'Claude' : kinds[0] === 'codex' ? 'Codex' : 'Grok'
    return `${hostLabel(host)} 缺少 ${kindLabel} CLI`
  }
  const hostCount = entries.length
  const cliCount = entries.reduce((n, [, ks]) => n + ks.length, 0)
  return `${hostCount} 台主機缺少 ${cliCount} 個 CLI`
}

/** Missing tools that affect the current bot or group members. */
export function relevantMissing(
  all: { host: string; kind: BotKind }[],
  focusHost: string | null | undefined,
  focusKinds: BotKind[] | null | undefined,
): { host: string; kind: BotKind }[] {
  if (!focusHost || !focusKinds || focusKinds.length === 0) return []
  const want = new Set(focusKinds)
  return all.filter((m) => m.host === focusHost && want.has(m.kind))
}

/** Stable snapshot key — `missingTools()` returns fresh objects each call. */
function missingKeyOf(state: Parameters<typeof missingTools>[0]): string {
  return missingTools(state)
    .map((m) => `${m.host}:${m.kind}`)
    .join('|')
}

function missingFromKey(key: string): { host: string; kind: BotKind }[] {
  if (!key) return []
  return key.split('|').map((part) => {
    const i = part.lastIndexOf(':')
    return { host: part.slice(0, i), kind: part.slice(i + 1) as BotKind }
  })
}

/** Compact amber icon + count for the main header when the full bar is not shown. */
export function ToolsHintIcon() {
  const missingKey = useStore((s) => missingKeyOf(s))
  const dismissed = useStore((s) => s.toolHintDismissed)
  const selectedBotId = useStore((s) => s.selectedBotId)
  const selectedProjectId = useStore((s) => s.selectedProjectId)
  const bots = useStore((s) => s.bots)
  const missing = useMemo(() => missingFromKey(missingKey), [missingKey])

  const focus = useMemo(() => {
    const state = useStore.getState()
    if (selectedProjectId) {
      const host = projectHostName(state, selectedProjectId)
      const kinds = bots.filter((b) => b.project_id === selectedProjectId).map((b) => b.kind)
      return { host, kinds: [...new Set(kinds)] as BotKind[] }
    }
    if (selectedBotId) {
      const bot = bots.find((b) => b.id === selectedBotId)
      if (!bot) return null
      return { host: projectHostName(state, bot.project_id), kinds: [bot.kind] as BotKind[] }
    }
    return null
  }, [selectedBotId, selectedProjectId, bots])

  const relevant = relevantMissing(missing, focus?.host, focus?.kinds)
  if (dismissed || missing.length === 0 || relevant.length > 0) return null

  const title = summarizeMissing(
    missing.reduce((map, m) => {
      map.set(m.host, [...(map.get(m.host) ?? []), m.kind])
      return map
    }, new Map<string, BotKind[]>()),
  )

  return (
    <span className="tools-hint-icon" title={title} aria-label={title}>
      <span aria-hidden="true">⚠</span>
      <span className="tools-hint-count">{missing.length}</span>
    </span>
  )
}

/** Banner under the header: only when the current bot / group recipients are affected. */
export function ToolsHint({
  focusHost,
  focusKinds,
}: {
  focusHost?: string | null
  focusKinds?: BotKind[] | null
}) {
  const missingKey = useStore((s) => missingKeyOf(s))
  const missingAll = useMemo(() => missingFromKey(missingKey), [missingKey])
  const dismissed = useStore((s) => s.toolHintDismissed)
  const dismiss = useStore((s) => s.dismissToolHint)
  const [collapsed, setCollapsed] = useState(true)

  const missing = relevantMissing(missingAll, focusHost, focusKinds)
  if (dismissed || missing.length === 0) return null

  const byHost = new Map<string, BotKind[]>()
  for (const m of missing) {
    byHost.set(m.host, [...(byHost.get(m.host) ?? []), m.kind])
  }
  const summary = summarizeMissing(byHost)

  return (
    <div className={`tools-hint${collapsed ? ' collapsed' : ''}`} role="status">
      <button type="button" className="tools-hint-toggle" aria-expanded={!collapsed} onClick={() => setCollapsed(!collapsed)} title={collapsed ? '展開' : '收合'}>
        <span className="chev">{collapsed ? '▶' : '▼'}</span>
        <span className="tools-hint-title">{summary}</span>
      </button>
      {collapsed ? null : (
        <ul className="tools-hint-list">
          {[...byHost.entries()].map(([host, kinds]) =>
            kinds.map((kind) => (
              <li key={`${host}:${kind}`}>
                <span>
                  主機 <strong>{hostLabel(host)}</strong> 缺少 <KindTag kind={kind} /> <strong>{kind}</strong>
                </span>
                <InstallToolButton host={host} kind={kind} />
              </li>
            )),
          )}
        </ul>
      )}
      <button type="button" className="icon-btn" onClick={dismiss} aria-label="關閉提示" title="這次不再提示">
        ✕
      </button>
    </div>
  )
}

/** Tools of the host a project sits on (used by the new-bot form to disable missing kinds). */
export function useHostTools(host: string): ToolMap {
  return useStore((s) => toolsOfHost(s, host))
}
