/**
 * 環境設定 →「OpenAI API」區（使用者 2026-11-18：「要跟 Claude 一樣可以設定」）：啟用開關（`[openai_api] enabled`）、
 * 金鑰狀態與 key_file 路徑、總花費上限與起算時間（`[openai_api.api_bot]`）、OpenAI 累計花費。
 * **不顯示、不回傳 key 內容**：只看得到「已設定／讀不到＋原因」；key 請放進 key_file 指到的檔（權限 600）。
 */
import { useEffect, useState } from 'react'
import { fetchOpenAiApiSettings, saveOpenAiApiSettings, type OpenAiApiPatch, type OpenAiApiSettings as Settings } from '../api/openaiApi'
import { useT } from '../lib/i18n'
import { usd } from './messageTranslationConstants'
import './claudeApiPanel.css'

export function OpenAiApiSettings() {
  const t = useT()
  const [settings, setSettings] = useState<Settings | null | undefined>(undefined)
  const [enabled, setEnabled] = useState(false)
  const [keyFile, setKeyFile] = useState('')
  const [cap, setCap] = useState('')
  const [turnCap, setTurnCap] = useState('')
  const [busy, setBusy] = useState(false)
  const [note, setNote] = useState<{ ok: boolean; text: string } | null>(null)

  const adopt = (s: Settings) => {
    setSettings(s)
    setEnabled(s.enabled)
    setKeyFile(s.key_file)
    setCap(s.total_cap_usd === null ? '' : String(s.total_cap_usd))
    setTurnCap(s.max_turn_cost_microusd === null ? '' : String(s.max_turn_cost_microusd / 1_000_000))
  }

  useEffect(() => {
    let alive = true
    void fetchOpenAiApiSettings()
      .then((s) => {
        if (!alive) return
        if (s) adopt(s)
        else setSettings(null)
      })
      .catch(() => {
        if (alive) setSettings(null)
      })
    return () => {
      alive = false
    }
  }, [])

  if (settings === undefined) return <p className="hint">{t('讀取中…')}</p>
  if (settings === null) {
    return (
      <p className="hint" data-testid="openai-api-unsupported">
        {t('OpenAI API 設定需要更新 daemon。')}
      </p>
    )
  }

  const capNumber = cap.trim() === '' ? null : Number(cap)
  const capInvalid = capNumber !== null && (!Number.isFinite(capNumber) || capNumber < 0)
  const savedCap = settings.total_cap_usd
  // 單回合上限：輸入美元、存微美元；留空＝未設定（清除），必須大於 0。
  const turnNumber = turnCap.trim() === '' ? null : Number(turnCap)
  const turnMicro = turnNumber === null || !Number.isFinite(turnNumber) ? null : Math.round(turnNumber * 1_000_000)
  const turnInvalid = turnNumber !== null && (!Number.isFinite(turnNumber) || (turnMicro ?? 0) < 1)
  const patch: OpenAiApiPatch = {}
  if (enabled !== settings.enabled) patch.enabled = enabled
  if (keyFile.trim() !== settings.key_file) patch.key_file = keyFile.trim()
  if (!capInvalid && capNumber !== savedCap) patch.total_cap_usd = capNumber
  if (!turnInvalid && turnMicro !== settings.max_turn_cost_microusd) patch.max_turn_cost_microusd = turnMicro
  const dirty = Object.keys(patch).length > 0

  const save = () => {
    setBusy(true)
    setNote(null)
    void saveOpenAiApiSettings(patch)
      .then((r) => {
        if (r.ok) {
          adopt(r.settings)
          setNote({ ok: true, text: t('已儲存，立即生效。') })
        } else {
          setNote({ ok: false, text: t('儲存失敗：{message}', { message: r.message }) })
        }
      })
      .finally(() => setBusy(false))
  }

  return (
    <form
      className="form claude-api-panel openai-api-settings"
      data-testid="openai-api-settings"
      onSubmit={(e) => {
        e.preventDefault()
        if (!busy && dirty && !capInvalid && !turnInvalid) save()
      }}
    >
      <p className="hint">{t('OpenAI 只走 API（按 API 計費）：翻譯可改用 OpenAI，之後的顧問型 bot 也用這裡的金鑰與上限。預設關閉。')}</p>
      <label className="claude-api-switch">
        <input type="checkbox" data-testid="openai-api-enabled" checked={enabled} disabled={!settings.key_present && !enabled} onChange={(e) => setEnabled(e.target.checked)} />
        <span>{t('啟用 OpenAI API（總開關）')}</span>
      </label>
      <label className="field">
        <span>
          {t('OpenAI API key')}{' '}
          <em data-testid="openai-api-key-status" className={settings.key_present ? 'claude-api-key ok' : 'claude-api-key'}>
            {settings.key_present ? t('已設定') : settings.key_error ? t('未設定（{error}）', { error: settings.key_error }) : t('未設定')}
          </em>
        </span>
        <input type="text" data-testid="openai-api-key-file" value={keyFile} autoComplete="off" spellCheck={false} onChange={(e) => setKeyFile(e.target.value)} />
        <span className="hint">{t('key 放在這個路徑的檔案裡（權限 600，一行）；不進設定檔、也不會顯示在這裡。')}</span>
      </label>
      <label className="field">
        <span>{t('總花費上限（美元）')}</span>
        <input type="number" min={0} step="0.01" data-testid="openai-api-cap" value={cap} placeholder={t('不設上限')} onChange={(e) => setCap(e.target.value)} />
        <span className="hint" data-testid="openai-api-cap-since">
          {settings.total_cap_since ? t('從 {since} 起算（OpenAI 自己的上限，不佔 Claude 的額度）。', { since: settings.total_cap_since.slice(0, 19).replace('T', ' ') }) : t('留空＝不設上限；設定上限時從現在起算。')}
        </span>
        {capInvalid ? <span className="hint err">{t('上限要是 0 以上的數字。')}</span> : null}
      </label>
      <label className="field">
        <span>{t('單回合成本上限（美元）')}</span>
        <input type="number" min={0} step="0.01" data-testid="openai-api-turn-cap" value={turnCap} placeholder={t('不設上限')} onChange={(e) => setTurnCap(e.target.value)} />
        <span className="hint" data-testid="openai-api-turn-cap-hint">
          {t('單回合（一次回合）的花費上限，必須大於 0。留空＝不設上限。')}
        </span>
        {turnInvalid ? <span className="hint err" data-testid="openai-api-turn-cap-invalid">{t('單回合上限要大於 0；留空＝不設上限。')}</span> : null}
      </label>
      <p className="hint" data-testid="openai-api-spent">
        {t('OpenAI 累計花費 {spent}（bot {bots}、翻譯等一次性用途 {uses}）。', { spent: usd(settings.spent_usd), bots: usd(settings.spent_bots_usd), uses: usd(settings.spent_uses_usd) })}
      </p>
      <p className="hint" data-testid="openai-api-cap-scope">
        {t('bot 的花費上限只計 bot 用量，不含翻譯（與 Claude 相同，只算 bot 的 api_bot_attempts）。')}
      </p>
      <div className="form-actions">
        {note ? <span className={note.ok ? 'claude-api-note ok' : 'claude-api-note err'} role="status">{note.text}</span> : null}
        <button type="submit" className="btn primary" data-testid="openai-api-save" disabled={busy || !dirty || capInvalid || turnInvalid}>
          {t('儲存')}
        </button>
      </div>
    </form>
  )
}
