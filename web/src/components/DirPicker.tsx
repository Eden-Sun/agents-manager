import { useEffect, useState } from 'react'
import { listDirs } from '../api'
import type { DirListing } from '../api/types'

/**
 * Server-backed directory browser. Browsers cannot hand a page a real filesystem path,
 * so the daemon lists directories (`GET /api/fs/dirs`) and the user drills down here.
 * With `host` set (SPEC §11.5) the daemon runs the listing over ssh on that host instead.
 */
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
  const [manual, setManual] = useState('')
  const remote = host && host !== 'local' ? host : null

  const load = (path?: string) => {
    setBusy(true)
    setError(null)
    listDirs(path, host)
      .then((l) => {
        setListing(l)
        setManual(l.path)
      })
      .catch((e: unknown) => setError(e instanceof Error ? e.message : String(e)))
      .finally(() => setBusy(false))
  }

  useEffect(() => {
    load(initial?.trim() || undefined)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [host])

  const crumbs = (() => {
    const p = listing?.path ?? ''
    if (!p) return [] as { label: string; path: string }[]
    const parts = p.split('/').filter(Boolean)
    const out: { label: string; path: string }[] = [{ label: '/', path: '/' }]
    let acc = ''
    for (const part of parts) {
      acc += `/${part}`
      out.push({ label: part, path: acc })
    }
    return out
  })()

  return (
    <div className="dirpicker" role="dialog" aria-label="選擇目錄">
      {remote ? (
        <div className="dirpicker-host" title={`透過 ssh 列出 ${remote} 上的目錄（SPEC §11.5）`}>
          <span className="host-badge">@{remote}</span>
          <span>遠端目錄</span>
        </div>
      ) : null}
      <div className="dirpicker-top">
        <button type="button" className="btn" title="上一層" disabled={!listing?.parent || busy} onClick={() => load(listing?.parent ?? undefined)}>
          ↑
        </button>
        <button type="button" className="btn" title="家目錄" disabled={busy} onClick={() => load(listing?.home || undefined)}>
          ⌂
        </button>
        <form
          className="dirpicker-manual"
          onSubmit={(e) => {
            e.preventDefault()
            if (manual.trim()) load(manual.trim())
          }}
        >
          <input
            type="text"
            value={manual}
            spellCheck={false}
            placeholder="/path/to/dir"
            onChange={(e) => setManual(e.target.value)}
          />
        </form>
      </div>
      <div className="dirpicker-crumbs">
        {crumbs.map((c, i) => (
          <span key={c.path}>
            {i > 0 && i < crumbs.length && c.label !== '/' ? <span className="sep">/</span> : null}
            <button type="button" className="crumb" disabled={busy} onClick={() => load(c.path)}>
              {c.label}
            </button>
          </span>
        ))}
      </div>
      <div className="dirpicker-list">
        {error ? <div className="dirpicker-empty err">{error}</div> : null}
        {!error && listing && listing.entries.length === 0 ? <div className="dirpicker-empty">（沒有子目錄）</div> : null}
        {listing?.entries.map((e) => (
          <button
            key={e.path}
            type="button"
            className="dirpicker-row"
            disabled={busy}
            onDoubleClick={() => onPick(e.path)}
            onClick={() => load(e.path)}
            title={e.path}
          >
            <span className="ico">📁</span>
            <span className="name">{e.name}</span>
            {e.git ? <span className="tag">git</span> : null}
          </button>
        ))}
      </div>
      <div className="form-actions">
        <span className="hint dirpicker-cur" title={listing?.path}>
          {listing?.path ?? ''}
        </span>
        <button type="button" className="btn" onClick={onCancel}>
          取消
        </button>
        <button type="button" className="btn primary" disabled={!listing || busy} onClick={() => listing && onPick(listing.path)}>
          選擇此目錄
        </button>
      </div>
    </div>
  )
}
