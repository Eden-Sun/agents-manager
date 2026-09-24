/**
 * 環境設定 →「Jev 第二意見」（SPEC §4.3c，issue #240）：貼 API key、開關、選哪些專案。
 *
 * 開了之後，這些專案的撞限畫面尾段（遮罩後）會送到 TypeSafe——所以開關旁邊直接寫清楚，不藏在說明裡。
 * key 貼上存檔後就清空輸入框；daemon 不會把它回給前端，這裡只看得到「已設定」。
 */
import { useEffect, useState } from 'react'
import { fetchJudgeSettings, saveJudgeSettings, type JudgeSettings } from '../api/judge'
import { useStore } from '../store/store'
import './judgePanel.css'

/**
 * 還沒讀到設定時這一格顯示什麼（issue #466）。三種狀態分開，不能共用同一句話：
 * `loading` 是還在讀，`unsupported` 是這顆 daemon 真的沒有這個端點（404／405），
 * `error` 是讀失敗——以前沒有第三種，daemon 斷線時 promise reject 沒有人接，
 * `settings` 永遠停在 `undefined`，整格就卡在「讀取中…」不動也不會重試。
 */
export type JudgeLoad = { kind: 'loading' } | { kind: 'unsupported' } | { kind: 'error'; message: string }

export function JudgeLoadNotice({ state, onRetry }: { state: JudgeLoad; onRetry: () => void }) {
  if (state.kind === 'loading') return <p className="hint">讀取中…</p>
  if (state.kind === 'unsupported') return <p className="hint">這顆 daemon 還沒有這個功能（需要更新 daemon）。</p>
  return (
    <p className="hint">
      讀不到 Jev 設定：{state.message}{' '}
      <button type="button" className="btn" onClick={onRetry}>
        重試
      </button>
    </p>
  )
}

export function JudgePanel() {
  const allProjects = useStore((s) => s.projects)
  const [settings, setSettings] = useState<JudgeSettings | null | undefined>(undefined)
  const [loadError, setLoadError] = useState<string | null>(null)
  /** 按「重試」就 +1，讓下面那個 effect 重跑一次。 */
  const [reload, setReload] = useState(0)
  const [enabled, setEnabled] = useState(false)
  const [picked, setPicked] = useState<string[]>([])
  const [token, setToken] = useState('')
  const [busy, setBusy] = useState(false)
  const [note, setNote] = useState<{ ok: boolean; text: string } | null>(null)

  const adopt = (s: JudgeSettings) => {
    setSettings(s)
    setEnabled(s.enabled)
    setPicked(s.projects)
  }

  useEffect(() => {
    let alive = true
    setLoadError(null)
    void fetchJudgeSettings()
      .then((s) => {
        if (!alive) return
        if (s) adopt(s)
        else setSettings(null)
      })
      // 少了這一條，daemon 沒開／網路斷掉時 promise reject 沒有人接，`settings` 永遠是
      // `undefined`，這一格就卡在「讀取中…」——而且 effect 的依賴不會再變，不會重試（issue #466）。
      .catch((e: unknown) => {
        if (!alive) return
        setLoadError(e instanceof Error ? e.message : String(e))
      })
    return () => {
      alive = false
    }
  }, [reload])

  // 錯誤要排在 `undefined` 前面：讀失敗時 `settings` 本來就還是 `undefined`。
  if (loadError !== null || settings === undefined) {
    const state: JudgeLoad = loadError !== null ? { kind: 'error', message: loadError } : { kind: 'loading' }
    return (
      <JudgeLoadNotice
        state={state}
        onRetry={() => {
          setSettings(undefined)
          setLoadError(null)
          setReload((n) => n + 1)
        }}
      />
    )
  }
  if (settings === null) return <JudgeLoadNotice state={{ kind: 'unsupported' }} onRetry={() => setReload((n) => n + 1)} />

  // 名單裡可能是 id 也可能是 label（手寫 config）；勾選一律存 id。
  const isPicked = (p: { id: string; label: string }) => picked.includes(p.id) || picked.includes(p.label)
  const toggle = (p: { id: string; label: string }) =>
    setPicked((cur) => (isPicked(p) ? cur.filter((x) => x !== p.id && x !== p.label) : [...cur, p.id]))
  const willHaveKey = settings.key_present || token.trim() !== ''
  const dirty = enabled !== settings.enabled || token.trim() !== '' || [...picked].sort().join('\n') !== [...settings.projects].sort().join('\n')

  const save = () => {
    setBusy(true)
    setNote(null)
    void saveJudgeSettings({ enabled, projects: picked, token: token.trim() || undefined })
      .then((r) => {
        if (r.ok) {
          adopt(r.settings)
          setToken('')
          setNote({ ok: true, text: '已儲存，立即生效。' })
        } else {
          setNote({ ok: false, text: `儲存失敗：${r.message}` })
        }
      })
      .catch((e: unknown) => setNote({ ok: false, text: `儲存失敗：${e instanceof Error ? e.message : String(e)}` }))
      .finally(() => setBusy(false))
  }

  return (
    <form
      className="form judge-panel"
      onSubmit={(e) => {
        e.preventDefault()
        if (!busy && dirty) save()
      }}
    >
      <p className="hint">
        畫面比對判定 codex／grok 撞限時，另外問 TypeSafe 的 Jev「這一行是介面畫的，還是 bot 印出來的內容」。只記錄、不改任何行為。
      </p>
      <label className="field">
        <span>
          TypeSafe API key{' '}
          <em className={settings.key_present ? 'judge-key ok' : 'judge-key'}>{settings.key_present ? '已設定' : `未設定${settings.key_error ? `（${settings.key_error}）` : ''}`}</em>
        </span>
        <input
          type="password"
          value={token}
          autoComplete="off"
          spellCheck={false}
          placeholder={settings.key_present ? '留空＝不更換' : '貼上 key'}
          onChange={(e) => setToken(e.target.value)}
        />
        <span className="hint">只寫進這台機器的 key 檔（權限 600），不進設定檔、不會再顯示。</span>
      </label>
      <label className="judge-switch">
        <input type="checkbox" checked={enabled} disabled={!willHaveKey && !enabled} onChange={(e) => setEnabled(e.target.checked)} />
        <span>啟用（會把下面專案的撞限畫面最後 60 行，遮罩 token／密碼後送到 TypeSafe）</span>
      </label>
      <fieldset className="judge-projects" disabled={!enabled}>
        <legend>適用的專案（沒勾＝不送）</legend>
        {allProjects.length === 0 ? <span className="hint">還沒有專案。</span> : null}
        {allProjects.map((p) => (
          <label key={p.id}>
            <input type="checkbox" checked={isPicked(p)} onChange={() => toggle(p)} />
            <span>{p.label}</span>
          </label>
        ))}
      </fieldset>
      <div className="form-actions">
        {note ? <span className={note.ok ? 'judge-note ok' : 'judge-note err'} role="status">{note.text}</span> : null}
        <button type="submit" className="btn primary" disabled={busy || !dirty}>
          儲存
        </button>
      </div>
    </form>
  )
}
