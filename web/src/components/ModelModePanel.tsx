import { useEffect, useState } from 'react'
import { fetchModelMode, setModelMode, type ModelModeState } from '../api/modelMode'
import { useT } from '../lib/i18n'

/** 環境設定→顯示：模型用法模式。選項與說明都來自 daemon；改完要重啟已在跑的 bot 才套用新規則。 */
export function ModelModePanel() {
  const t = useT()
  const [state, setState] = useState<ModelModeState | null | undefined>(undefined)
  const [loadError, setLoadError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const [note, setNote] = useState<{ ok: boolean; text: string } | null>(null)

  useEffect(() => {
    let alive = true
    void fetchModelMode()
      .then((s) => alive && setState(s))
      .catch((e: unknown) => alive && setLoadError(e instanceof Error ? e.message : String(e)))
    return () => {
      alive = false
    }
  }, [])

  if (state === null) return null
  if (loadError !== null) return <p className="hint">{t('讀不到模型用法模式：{message}', { message: loadError })}</p>
  if (state === undefined) return <p className="hint">{t('讀取中…')}</p>

  const pick = (mode: string) => {
    if (busy || mode === state.mode) return
    setBusy(true)
    setNote(null)
    void setModelMode(mode)
      .then((r) => {
        if (r.ok) {
          setState({ ...state, mode: r.mode })
          setNote({ ok: true, text: r.needsRestartBots ? t('已儲存。已在跑的 bot 要重啟才套用新規則。') : t('已儲存。') })
        } else {
          setNote({ ok: false, text: t('儲存失敗：{message}', { message: r.message }) })
        }
      })
      .catch((e: unknown) => setNote({ ok: false, text: t('儲存失敗：{message}', { message: e instanceof Error ? e.message : String(e) }) }))
      .finally(() => setBusy(false))
  }

  return (
    <fieldset className="model-mode" disabled={busy}>
      <legend>{t('模型用法模式')}</legend>
      {state.modes.map((m) => (
        <label key={m.id} className="model-mode-row">
          <input type="radio" name="model-mode" value={m.id} checked={state.mode === m.id} onChange={() => pick(m.id)} />
          <span>
            <strong>{m.label}</strong>
            <span className="model-mode-desc">{m.description}</span>
          </span>
        </label>
      ))}
      {note ? (
        <p className={note.ok ? 'model-mode-note ok' : 'model-mode-note err'} role="status">
          {note.text}
        </p>
      ) : null}
    </fieldset>
  )
}
