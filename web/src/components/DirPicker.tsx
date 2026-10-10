import { useEffect, useId, useMemo, useRef, useState, type KeyboardEvent as ReactKeyboardEvent } from 'react'
import { createDir, listDirs } from '../api'
import { useMediaQuery } from '../hooks/useMediaQuery'
import { createLatestOnly } from '../lib/latestOnly'
import { isImeEnter } from '../lib/ime'
import { keyBelongsToControl } from '../lib/domEvents'
import { ApiError, type DirListing } from '../api/types'
import { newDirNameProblem } from '../lib/newDirName'
import './dirPicker.css'

/**
 * Server-backed directory browser (browsers cannot expose real paths); with `host`, listed over ssh (SPEC §11.5).
 * Interaction follows the macOS open panel: click highlights, double-click / Enter / → walks in.
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
  /** 「新資料夾」的內嵌表單（issue #877）：在目前瀏覽的這一層建一個資料夾，建好直接選取。 */
  const [creating, setCreating] = useState(false)
  const [newName, setNewName] = useState('')
  const [createError, setCreateError] = useState<string | null>(null)
  const [createBusy, setCreateBusy] = useState(false)
  const remote = host && host !== 'local' ? host : null

  const filterRef = useRef<HTMLInputElement>(null)
  const manualRef = useRef<HTMLInputElement>(null)
  const newNameRef = useRef<HTMLInputElement>(null)
  const listRef = useRef<HTMLDivElement>(null)
  // 觸控裝置點一下會叫出軟鍵盤：焦點不自動搬進過濾框（鍵盤操作不受影響，桌機照舊留在過濾框）。
  const coarse = useMediaQuery('(pointer: coarse)')
  const listId = useId()
  const optId = (i: number) => `${listId}-opt-${i}`

  // 手動輸入路徑的表單不看 `busy`，可以疊送：只採最後一次的結果，不然停在舊路徑、選到不是最後輸入的目錄。
  const latest = useRef(createLatestOnly()).current
  const load = (path?: string, opts?: { hidden?: boolean; keep?: string }) => {
    const ticket = latest.begin()
    setBusy(true)
    setError(null)
    listDirs(path, host, opts?.hidden ?? hidden)
      .then((l) => {
        if (!latest.isCurrent(ticket)) return
        setListing(l)
        setManual(l.path)
        setEditing(false)
        setFilter('')
        // Coming back up a level, land on the child we just left — that is where the eye is.
        const back = opts?.keep ? l.entries.findIndex((e) => e.path === opts.keep) : -1
        setSel(back)
        if (!coarse) filterRef.current?.focus()
      })
      .catch((e: unknown) => {
        if (latest.isCurrent(ticket)) setError(e instanceof Error ? e.message : String(e))
      })
      .finally(() => {
        if (latest.isCurrent(ticket)) setBusy(false)
      })
  }

  useEffect(() => {
    load(initial?.trim() || undefined)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [host])

  useEffect(() => {
    if (editing) manualRef.current?.select()
  }, [editing])
  useEffect(() => {
    if (creating) newNameRef.current?.focus()
  }, [creating])

  const closeCreate = () => {
    setCreating(false)
    setNewName('')
    setCreateError(null)
    if (!coarse) filterRef.current?.focus()
  }
  const submitCreate = () => {
    if (!listing || createBusy) return
    // 這一層已經列得到同名的（不分大小寫也提示，macOS／Windows 的檔案系統會撞）：不必等 daemon 回 409。
    const problem =
      newDirNameProblem(newName) ??
      (listing.entries.some((e) => e.name.toLowerCase() === newName.trim().toLowerCase()) ? `「${newName.trim()}」已經存在，換一個名稱，或直接選那個資料夾` : null)
    if (problem) {
      setCreateError(problem)
      return
    }
    setCreateBusy(true)
    setCreateError(null)
    createDir(listing.path, newName.trim(), host)
      .then((made) => {
        // 建好就是要它：直接當 Project 路徑，不再多走一步。
        onPick(made.path)
      })
      .catch((e: unknown) => {
        // 409 的訊息在 body.message（`ApiError.message` 優先講 reason，那是機器碼）。
        setCreateError(e instanceof ApiError && typeof e.body.message === 'string' ? e.body.message : e instanceof Error ? e.message : String(e))
      })
      .finally(() => setCreateBusy(false))
  }

  const entries = useMemo(() => {
    const q = filter.trim().toLowerCase()
    const all = listing?.entries ?? []
    if (!q) return all
    return all.filter((e) => e.name.toLowerCase().includes(q))
  }, [listing, filter])

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
    // 輸入法選字中的 Esc 是取消選字：不收表單、不清過濾、不離開。preventDefault 是給外層 Modal 看的（它見 defaultPrevented 就不關）。
    if (e.key === 'Escape' && isImeEnter(e.nativeEvent)) {
      e.preventDefault()
      return
    }
    // busy 時只擋其他鍵；Esc 照常離開並 preventDefault，不然外層 Modal 會把整個新增視窗關掉。
    if (busy && e.key !== 'Escape') return
    if (creating) {
      // 名稱輸入框自己管鍵盤；只有 Esc 退回清單。
      if (e.key === 'Escape') {
        e.preventDefault()
        closeCreate()
      }
      return
    }
    if (editing) {
      // The path field owns its own keys; only Esc escapes back to the crumb bar.
      if (e.key === 'Escape') {
        e.preventDefault()
        setManual(listing?.path ?? '')
        setEditing(false)
      }
      return
    }
    // 焦點在按鈕／勾選格上時，Enter、方向鍵、Backspace 是那個控制項的（否則按「取消」會變成選擇目錄）；Esc 照舊離開。
    if (e.key !== 'Escape' && keyBelongsToControl(e.target)) return
    if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
      e.preventDefault()
      if (!entries.length) return
      const d = e.key === 'ArrowDown' ? 1 : -1
      setSel((s) => (s < 0 ? (d > 0 ? 0 : entries.length - 1) : (s + d + entries.length) % entries.length))
      return
    }
    if (e.key === 'Enter') {
      if (isImeEnter(e.nativeEvent)) return
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
        <button type="button" className="btn icon" aria-label="上一層" title="上一層（←）" disabled={!listing?.parent || busy} onClick={up}>
          ↑
        </button>
        <button
          type="button"
          className="btn icon"
          aria-label="家目錄"
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
                  title={c.path}
                  onClick={() => load(c.path)}
                >
                  {c.label}
                </button>
              </span>
            ))}
            <button
              type="button"
              className="crumb-edit"
              aria-label="手動輸入路徑"
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
          // 焦點留在這格、↑/↓ 移 highlight，靠 aria-activedescendant 告訴螢幕閱讀器。
          role="combobox"
          aria-label="篩選這層資料夾"
          aria-expanded={entries.length > 0}
          aria-controls={listId}
          aria-autocomplete="list"
          aria-activedescendant={selected ? optId(sel) : undefined}
          aria-describedby={`${listId}-keys`}
          onChange={(e) => {
            setFilter(e.target.value)
            setSel(e.target.value ? 0 : -1)
          }}
        />
        <button
          type="button"
          className="btn dirpicker-newbtn"
          aria-expanded={creating}
          title="在這一層建一個新資料夾，建好直接選取"
          disabled={!listing || busy || creating}
          onClick={() => setCreating(true)}
        >
          ＋ 新資料夾
        </button>
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

      {creating ? (
        <form
          className="dirpicker-new"
          aria-label="新資料夾"
          onSubmit={(e) => {
            e.preventDefault()
            submitCreate()
          }}
        >
          <div className="dirpicker-new-row">
            <input
              ref={newNameRef}
              type="text"
              value={newName}
              spellCheck={false}
              disabled={createBusy}
              placeholder="新資料夾名稱"
              aria-label="新資料夾名稱"
              aria-invalid={createError ? true : undefined}
              onChange={(e) => {
                setNewName(e.target.value)
                setCreateError(null)
              }}
              onKeyDown={(e) => {
                // 輸入法選字的 Enter 不是送出。
                if (e.key === 'Enter' && isImeEnter(e.nativeEvent)) e.preventDefault()
              }}
            />
            <button type="submit" className="btn primary" disabled={createBusy || !newName.trim()}>
              {createBusy ? '建立中…' : '建立並選取'}
            </button>
            <button type="button" className="btn" disabled={createBusy} onClick={closeCreate}>
              取消
            </button>
          </div>
          <div className="dirpicker-new-hint" title={listing?.path}>
            建在 {listing?.path}
          </div>
          {createError ? (
            <div className="dirpicker-empty err" role="alert">
              {createError}
            </div>
          ) : null}
        </form>
      ) : null}

      {/* option 裡不能有按鈕，› 只是滑鼠捷徑；沒有資料夾時不掛 listbox。 */}
      <div className="dirpicker-list" ref={listRef} id={listId} role={entries.length > 0 ? 'listbox' : undefined} aria-label="子資料夾">
        {error ? <div className="dirpicker-empty err">{error}</div> : null}
        {!error && busy && !listing ? <div className="dirpicker-empty">載入中…</div> : null}
        {!error && listing && listing.entries.length === 0 ? (
          <div className="dirpicker-empty">（沒有子資料夾，可直接選擇這一層）</div>
        ) : null}
        {!error && listing?.truncated ? (
          <div className="dirpicker-empty">這個資料夾的子資料夾太多，只列出前 2000 個；上面的輸入框只過濾這幾個，要找別的請直接貼完整路徑。</div>
        ) : null}
        {!error && listing && listing.entries.length > 0 && entries.length === 0 ? (
          <div className="dirpicker-empty">沒有符合「{filter}」的資料夾</div>
        ) : null}
        {entries.map((e, i) => (
          <div
            key={e.path}
            id={optId(i)}
            data-idx={i}
            role="option"
            aria-selected={i === sel}
            aria-disabled={busy || undefined}
            className={`dirpicker-row${i === sel ? ' sel' : ''}`}
            onMouseDown={(ev) => {
              // 觸控：不攔 mousedown，讓點一列把焦點從過濾框移走（鍵盤收起來）。
              if (coarse) return
              ev.preventDefault()
              filterRef.current?.focus()
            }}
            onClick={() => {
              if (!busy) setSel(i)
            }}
            onDoubleClick={() => {
              if (!busy) enter(e.path)
            }}
          >
            <span className="dirpicker-pick">
              <span className="ico" aria-hidden="true">📁</span>
              <span className="name">{e.name}</span>
              {e.git ? <span className="tag">git</span> : null}
            </span>
            <span
              className="dirpicker-enter"
              title={`進入 ${e.name}`}
              aria-hidden="true"
              onClick={(ev) => {
                ev.stopPropagation()
                if (!busy) enter(e.path)
              }}
            >
              ›
            </span>
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
        <div className="dirpicker-keys" id={`${listId}-keys`}>↩ 進入 · ⌘↩ 直接選擇 · ← 上一層 · ↑↓ 移動</div>
        <div className="dirpicker-actions">
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
