import { useEffect, useMemo, useRef, useState, type KeyboardEvent as ReactKeyboardEvent } from 'react'
import { listDirs } from '../api'
import type { DirListing } from '../api/types'

/**
 * Server-backed directory browser. Browsers cannot hand a page a real filesystem path,
 * so the daemon lists directories (`GET /api/fs/dirs`) and the user drills down here.
 * With `host` set (SPEC §11.5) the daemon runs the listing over ssh on that host instead.
 *
 * Interaction follows the macOS open panel: one click highlights a folder, double-click
 * (or Enter / →, or the row's ›) walks into it, and the primary button takes whatever is
 * highlighted — so a folder three levels down is one click away instead of three.
 */
/** Long folder names would otherwise stretch the primary button past the sidebar. */
function short(name: string) {
  return name.length > 14 ? `${name.slice(0, 13)}…` : name
}

export function DirPicker({
  initial,
  host,
  onPick,
  onCancel,
}: {
  initial?: string
  /** `undefined` / `"local"` = the daemon's own filesystem */
  host?: string
  onPick: (path: string) => void
  onCancel: () => void
}) {
  const [listing, setListing] = useState<DirListing | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const [hidden, setHidden] = useState(false)
  const [filter, setFilter] = useState('')
  const [sel, setSel] = useState(-1)
  /** The crumb bar turns into a text field while the user types a path by hand. */
  const [editing, setEditing] = useState(false)
  const [manual, setManual] = useState('')
  const remote = host && host !== 'local' ? host : null

  const filterRef = useRef<HTMLInputElement>(null)
  const manualRef = useRef<HTMLInputElement>(null)
  const listRef = useRef<HTMLDivElement>(null)

  const load = (path?: string, opts?: { hidden?: boolean; keep?: string }) => {
    setBusy(true)
    setError(null)
    listDirs(path, host, opts?.hidden ?? hidden)
      .then((l) => {
        setListing(l)
        setManual(l.path)
        setEditing(false)
        setFilter('')
        // Coming back up a level, land on the child we just left — that is where the eye is.
        const back = opts?.keep ? l.entries.findIndex((e) => e.path === opts.keep) : -1
        setSel(back)
        filterRef.current?.focus()
      })
      .catch((e: unknown) => setError(e instanceof Error ? e.message : String(e)))
      .finally(() => setBusy(false))
  }

  useEffect(() => {
    load(initial?.trim() || undefined)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [host])

  useEffect(() => {
    if (editing) manualRef.current?.select()
  }, [editing])

  const entries = useMemo(() => {
    const q = filter.trim().toLowerCase()
    const all = listing?.entries ?? []
    if (!q) return all
    return all.filter((e) => e.name.toLowerCase().includes(q))
  }, [listing, filter])

  // Keep the highlight inside the filtered list and scrolled into view.
  useEffect(() => {
    if (sel >= entries.length) setSel(entries.length ? 0 : -1)
  }, [entries.length, sel])
  useEffect(() => {
    if (sel < 0) return
    listRef.current?.querySelector<HTMLElement>(`[data-idx="${sel}"]`)?.scrollIntoView({ block: 'nearest' })
  }, [sel])

  const crumbs = useMemo(() => {
    const p = listing?.path ?? ''
    if (!p) return [] as { label: string; path: string }[]
    const home = listing?.home ?? ''
    // Collapse the home prefix into ⌂ so deep paths stay readable.
    const underHome = home && (p === home || p.startsWith(`${home}/`))
    const rest = underHome ? p.slice(home.length) : p
    const out: { label: string; path: string }[] = underHome
      ? [{ label: '⌂', path: home }]
      : [{ label: '/', path: '/' }]
    let acc = underHome ? home : ''
    for (const part of rest.split('/').filter(Boolean)) {
      acc += `/${part}`
      out.push({ label: part, path: acc })
    }
    return out
  }, [listing])

  const selected = sel >= 0 && sel < entries.length ? entries[sel] : null
  const target = selected?.path ?? listing?.path ?? ''
  const enter = (path: string) => load(path)
  const up = () => listing?.parent && load(listing.parent, { keep: listing.path })

  const onKeyDown = (e: ReactKeyboardEvent) => {
    if (busy) return
    if (editing) {
      // The path field owns its own keys; only Esc escapes back to the crumb bar.
      if (e.key === 'Escape') {
        e.preventDefault()
        setManual(listing?.path ?? '')
        setEditing(false)
      }
      return
    }
    if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
      e.preventDefault()
      if (!entries.length) return
      const d = e.key === 'ArrowDown' ? 1 : -1
      setSel((s) => (s < 0 ? (d > 0 ? 0 : entries.length - 1) : (s + d + entries.length) % entries.length))
      return
    }
    if (e.key === 'Enter') {
      e.preventDefault()
      // ⌘/Ctrl+Enter takes the highlighted folder without walking into it.
      if (e.metaKey || e.ctrlKey || !selected) onPick(target)
      else enter(selected.path)
      return
    }
    if (e.key === 'ArrowRight' && selected && !filter) {
      e.preventDefault()
      enter(selected.path)
      return
    }
    if ((e.key === 'ArrowLeft' || e.key === 'Backspace') && !filter) {
      e.preventDefault()
      up()
      return
    }
    if (e.key === 'Escape') {
      e.preventDefault()
      if (filter) setFilter('')
      else onCancel()
    }
  }

  return (
    // eslint-disable-next-line jsx-a11y/no-noninteractive-element-interactions
    <div className="dirpicker" role="dialog" aria-label="選擇目錄" onKeyDown={onKeyDown}>
      {remote ? (
        <div className="dirpicker-host" title={`透過 ssh 列出 ${remote} 上的目錄（SPEC §11.5）`}>
          <span className="host-badge">@{remote}</span>
          <span>遠端目錄</span>
        </div>
      ) : null}

      <div className="dirpicker-top">
        <button type="button" className="btn icon" title="上一層（←）" disabled={!listing?.parent || busy} onClick={up}>
          ↑
        </button>
        <button
          type="button"
          className="btn icon"
          title="家目錄"
          disabled={busy}
          onClick={() => load(listing?.home || undefined)}
        >
          ⌂
        </button>
        {editing ? (
          <form
            className="dirpicker-manual"
            onSubmit={(e) => {
              e.preventDefault()
              if (manual.trim()) load(manual.trim())
            }}
          >
            <input
              ref={manualRef}
              type="text"
              value={manual}
              spellCheck={false}
              placeholder="/path/to/dir 或 ~/dir"
              onChange={(e) => setManual(e.target.value)}
              onBlur={() => {
                setManual(listing?.path ?? '')
                setEditing(false)
              }}
            />
          </form>
        ) : (
          <div className="dirpicker-crumbs" role="navigation" aria-label="路徑">
            {crumbs.map((c, i) => (
              <span key={c.path} className="crumb-wrap">
                {i > 0 ? <span className="sep">/</span> : null}
                <button
                  type="button"
                  className={`crumb${i === crumbs.length - 1 ? ' cur' : ''}`}
                  disabled={busy}
                  onClick={() => load(c.path)}
                >
                  {c.label}
                </button>
              </span>
            ))}
            <button
              type="button"
              className="crumb-edit"
              title="手動輸入路徑"
              disabled={busy}
              onClick={() => setEditing(true)}
            >
              ✎
            </button>
          </div>
        )}
      </div>

      <div className="dirpicker-tools">
        <input
          ref={filterRef}
          type="text"
          className="dirpicker-filter"
          value={filter}
          spellCheck={false}
          autoFocus
          placeholder="篩選這層資料夾…"
          onChange={(e) => {
            setFilter(e.target.value)
            setSel(e.target.value ? 0 : -1)
          }}
        />
        <label className="dirpicker-hidden" title="也列出 .開頭 的資料夾">
          <input
            type="checkbox"
            checked={hidden}
            disabled={busy}
            onChange={(e) => {
              setHidden(e.target.checked)
              load(listing?.path, { hidden: e.target.checked })
            }}
          />
          <span>隱藏資料夾</span>
        </label>
      </div>

      <div className="dirpicker-list" ref={listRef} role="listbox" aria-label="子資料夾">
        {error ? <div className="dirpicker-empty err">{error}</div> : null}
        {!error && busy && !listing ? <div className="dirpicker-empty">載入中…</div> : null}
        {!error && listing && listing.entries.length === 0 ? (
          <div className="dirpicker-empty">（沒有子資料夾，可直接選擇這一層）</div>
        ) : null}
        {!error && listing && listing.entries.length > 0 && entries.length === 0 ? (
          <div className="dirpicker-empty">沒有符合「{filter}」的資料夾</div>
        ) : null}
        {entries.map((e, i) => (
          <div
            key={e.path}
            data-idx={i}
            role="option"
            aria-selected={i === sel}
            className={`dirpicker-row${i === sel ? ' sel' : ''}`}
            onDoubleClick={() => enter(e.path)}
          >
            <button type="button" className="dirpicker-pick" disabled={busy} onClick={() => setSel(i)}>
              <span className="ico">📁</span>
              <span className="name">{e.name}</span>
              {e.git ? <span className="tag">git</span> : null}
            </button>
            <button
              type="button"
              className="dirpicker-enter"
              title={`進入 ${e.name}`}
              aria-label={`進入 ${e.name}`}
              disabled={busy}
              onClick={() => enter(e.path)}
            >
              ›
            </button>
          </div>
        ))}
      </div>

      <div className="dirpicker-foot">
        <div className="dirpicker-target">
          <span className="lbl">選擇</span>
          <span className="hint dirpicker-cur" title={target}>
            {target}
          </span>
        </div>
        <div className="dirpicker-actions">
          <span className="dirpicker-keys">Enter 進入 · ⌘↩ 選擇 · ← 上一層</span>
          <button type="button" className="btn" onClick={onCancel}>
            取消
          </button>
          <button
            type="button"
            className="btn primary"
            disabled={!listing || busy}
            title={target}
            onClick={() => onPick(target)}
          >
            {selected ? `選擇「${short(selected.name)}」` : '選擇這一層'}
          </button>
        </div>
      </div>
    </div>
  )
}
