/**
 * 環境設定 →「Claude API」（SPEC §4.3d，issue #1919）：把一次性、不需要工具與 repo 的 LLM 工作改走 API key，
 * 不開 Claude Code session，省訂閱用量。貼 key、總開關、每個用途的開關／模型／effort／每小時上限，下面是用量帳本。
 * key 貼上存檔後就清空輸入框；daemon 不會把它回給前端，這裡只看得到「已設定」（比照 Jev，`JudgePanel`）。
 */
import { useCallback, useEffect, useState } from 'react'
import {
  fetchClaudeApiLedger,
  fetchClaudeApiSettings,
  saveClaudeApiSettings,
  type ClaudeApiLedger,
  type ClaudeApiSettings,
  type ClaudeApiUse,
  type ClaudeApiUsePatch,
} from '../api/claudeApi'
import { msg, useT } from '../lib/i18n'
import { EnvSubTabs, type EnvSubTabItem } from './EnvSubTabs'
import { JudgeLoadNotice, type JudgeLoad } from './JudgePanel'
import './claudeApiPanel.css'

const nf = new Intl.NumberFormat('en-US')

const ERROR_LABEL: Record<string, string> = {
  auth: msg('key 被拒'),
  rate_limited: msg('被限流'),
  server: msg('伺服器錯誤'),
  timeout: msg('逾時'),
  network: msg('連不上'),
  rejected: msg('請求被拒'),
  bad_response: msg('回應不能用'),
}

/** 這個用途現在表單上的值與已存的值是否不同。 */
function isUseDirty(draft: ClaudeApiUsePatch, saved: ClaudeApiUse): boolean {
  return (
    (draft.enabled ?? saved.enabled) !== saved.enabled ||
    (draft.model ?? saved.model) !== saved.model ||
    (draft.effort ?? saved.effort) !== saved.effort ||
    (draft.max_per_hour ?? saved.max_per_hour) !== saved.max_per_hour
  )
}

export function ClaudeApiPanel() {
  const t = useT()
  const [subTab, setSubTab] = useState<'form' | 'book'>('form')
  const [settings, setSettings] = useState<ClaudeApiSettings | null | undefined>(undefined)
  const [loadError, setLoadError] = useState<string | null>(null)
  const [reload, setReload] = useState(0)
  const [enabled, setEnabled] = useState(false)
  const [drafts, setDrafts] = useState<Record<string, ClaudeApiUsePatch>>({})
  const [token, setToken] = useState('')
  const [busy, setBusy] = useState(false)
  const [note, setNote] = useState<{ ok: boolean; text: string } | null>(null)
  const [ledger, setLedger] = useState<ClaudeApiLedger | null>(null)

  const adopt = (s: ClaudeApiSettings) => {
    setSettings(s)
    setEnabled(s.enabled)
    setDrafts({})
  }

  const loadLedger = useCallback(() => {
    void fetchClaudeApiLedger(8)
      .then(setLedger)
      .catch(() => setLedger(null))
  }, [])

  useEffect(() => {
    let alive = true
    void fetchClaudeApiSettings()
      .then((s) => {
        if (!alive) return
        if (s) adopt(s)
        else setSettings(null)
      })
      .catch((e: unknown) => {
        if (alive) setLoadError(e instanceof Error ? e.message : String(e))
      })
    loadLedger()
    return () => {
      alive = false
    }
  }, [reload, loadLedger])

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

  const willHaveKey = settings.key_present || token.trim() !== ''
  const patchUse = (id: string, p: ClaudeApiUsePatch) => setDrafts((cur) => ({ ...cur, [id]: { ...cur[id], ...p } }))
  const dirtyUses = settings.uses.filter((u) => isUseDirty(drafts[u.id] ?? {}, u))
  const dirty = enabled !== settings.enabled || token.trim() !== '' || dirtyUses.length > 0

  const save = () => {
    setBusy(true)
    setNote(null)
    const uses: Record<string, ClaudeApiUsePatch> = {}
    for (const u of dirtyUses) uses[u.id] = drafts[u.id] ?? {}
    void saveClaudeApiSettings({ enabled, uses, token: token.trim() || undefined })
      .then((r) => {
        if (r.ok) {
          adopt(r.settings)
          setToken('')
          setNote({ ok: true, text: t('已儲存，立即生效。') })
        } else {
          setNote({ ok: false, text: t('儲存失敗：{message}', { message: r.message }) })
        }
      })
      .catch((e: unknown) => setNote({ ok: false, text: t('儲存失敗：{message}', { message: e instanceof Error ? e.message : String(e) }) }))
      .finally(() => setBusy(false))
  }

  const modelOptions = (current: string) => (settings.models.includes(current) ? settings.models : [...settings.models, current])

  const formContent = (
    <form
      className="form claude-api-panel"
      onSubmit={(e) => {
        e.preventDefault()
        if (!busy && dirty) save()
      }}
    >
      <p className="hint">
        {t('一次性、不需要工具與 repo 的 LLM 工作（例如請 AGM 解析 changelog）改走 Anthropic API key，不開 Claude Code session，不吃訂閱用量；改按 API 計費。每個用途可以各自開關、選模型；失敗或沒 key 時，用途會退回原本的 Claude Code 路徑。')}
      </p>
      <label className="field">
        <span>
          Anthropic API key{' '}
          <em className={settings.key_present ? 'claude-api-key ok' : 'claude-api-key'}>
            {settings.key_present
              ? t('已設定')
              : settings.key_error
                ? t('未設定（{error}）', { error: settings.key_error })
                : t('未設定')}
          </em>
        </span>
        <input
          type="password"
          value={token}
          autoComplete="off"
          spellCheck={false}
          placeholder={settings.key_present ? t('留空＝不更換') : t('貼上 key（sk-ant-…）')}
          onChange={(e) => setToken(e.target.value)}
        />
        <span className="hint">{t('只寫進這台機器的 key 檔（權限 600），不進設定檔、不會再顯示。')}</span>
      </label>
      <label className="claude-api-switch">
        <input type="checkbox" checked={enabled} disabled={!willHaveKey && !enabled} onChange={(e) => setEnabled(e.target.checked)} />
        <span>{t('總開關（關掉時下面各用途都不會呼叫 API；「更新框翻譯」沿用舊設定，不受總開關管）')}</span>
      </label>
      <div className="claude-api-uses">
        {settings.uses.map((u) => {
          const d = drafts[u.id] ?? {}
          const on = d.enabled ?? u.enabled
          const model = d.model ?? u.model
          const effort = d.effort ?? u.effort
          const perHour = d.max_per_hour ?? u.max_per_hour
          return (
            <fieldset key={u.id} className="claude-api-use" data-use={u.id} disabled={!u.legacy_section && !enabled}>
              <legend>{u.label}</legend>
              <label className="claude-api-switch">
                <input type="checkbox" checked={on} disabled={!willHaveKey && !on} onChange={(e) => patchUse(u.id, { enabled: e.target.checked })} />
                <span>{t('啟用')}</span>
              </label>
              <div className="claude-api-use-fields">
                <label className="field">
                  <span>{t('模型')}</span>
                  <select value={model} onChange={(e) => patchUse(u.id, { model: e.target.value })}>
                    {modelOptions(model).map((m) => (
                      <option key={m} value={m}>
                        {m}
                        {m === u.default_model ? t('（預設）') : ''}
                      </option>
                    ))}
                  </select>
                </label>
                <label className="field">
                  <span>{t('思考強度')}</span>
                  <select value={effort} onChange={(e) => patchUse(u.id, { effort: e.target.value })}>
                    {settings.efforts.map((x) => (
                      <option key={x} value={x}>
                        {x}
                      </option>
                    ))}
                  </select>
                </label>
                <label className="field">
                  <span>{t('每小時上限')}</span>
                  <input
                    type="number"
                    min={u.id === 'changelog_translate' ? 0 : 1}
                    max={1000}
                    value={perHour}
                    onChange={(e) => {
                      const n = Number.parseInt(e.target.value, 10)
                      if (Number.isFinite(n)) patchUse(u.id, { max_per_hour: n })
                    }}
                  />
                  {u.id === 'changelog_translate' ? <small>{t('0＝不限')}</small> : null}
                </label>
              </div>
              {u.legacy_section ? <span className="hint">{t('設定存在 [{section}]，不受總開關管。', { section: u.legacy_section })}</span> : null}
            </fieldset>
          )
        })}
      </div>
      <div className="form-actions">
        {note ? <span className={note.ok ? 'claude-api-note ok' : 'claude-api-note err'} role="status">{note.text}</span> : null}
        <button type="submit" className="btn primary" disabled={busy || !dirty}>
          {t('儲存')}
        </button>
      </div>
    </form>
  )

  const ledgerContent = (
    <div className="claude-api-ledger">
      <h3 className="page-title">
        {t('用量帳本')}{' '}
        <button type="button" className="btn" onClick={loadLedger}>
          {t('重新整理')}
        </button>
      </h3>
      {ledger === null ? (
        <p className="hint">{t('讀不到帳本。')}</p>
      ) : (
        <>
          <p className="hint" data-testid="claude-api-totals">
            {t('自 daemon 啟動起共 {calls} 次（成功 {ok}、失敗 {errors}），輸入 {input}、輸出 {output} tokens。這些工作沒有走訂閱額度。', {
              calls: ledger.totals.calls,
              ok: ledger.totals.ok,
              errors: ledger.totals.errors,
              input: nf.format(ledger.totals.input_tokens),
              output: nf.format(ledger.totals.output_tokens),
            })}
          </p>
          {ledger.rows.length === 0 ? (
            <div className="hint" data-testid="claude-api-empty-ledger">
              <p>{t('還沒有呼叫記錄。')}</p>
              <p>{t('本帳本只保留目前 daemon 行程的記憶體記錄，重啟後會清空；不包含 Claude Code、Codex 或其他 CLI 聊天。')}</p>
              <p>{t('用途未啟用、key 無法使用或每小時額度已滿時，不會送出請求，也不會記入帳本。')}</p>
              {!settings.key_present && <p>{t('共用 API key 尚未就緒，請到「設定」查看原因。')}</p>}
              <p>
                {settings.uses.some((u) => u.enabled && (u.legacy_section !== null || settings.enabled))
                  ? t('目前已啟用的用途：{uses}。啟用不會立即呼叫 API，仍須觸發對應工作；既有快取可能不需再次呼叫。', {
                    uses: settings.uses.filter((u) => u.enabled && (u.legacy_section !== null || settings.enabled)).map((u) => u.label).join('、'),
                  })
                  : t('目前沒有已啟用的 API 用途（更新框翻譯依自己的開關計算）。')}
              </p>
              <p>{t('取得 changelog 失敗或沒有待處理版本時，也不會觸發分診 API。')}</p>
            </div>
          ) : (
            <div className="claude-api-ledger-scroll">
              <table className="as-cards">
                <thead>
                  <tr>
                    <th>{t('時間')}</th>
                    <th>{t('用途')}</th>
                    <th>{t('模型')}</th>
                    <th>{t('輸入')}</th>
                    <th>{t('輸出')}</th>
                    <th>{t('耗時')}</th>
                    <th>{t('結果')}</th>
                  </tr>
                </thead>
                <tbody>
                  {ledger.rows.map((r, i) => (
                    <tr key={`${r.at}-${i}`}>
                      <td className="num" data-label={t('時間')}>{r.at.slice(5, 19).replace('T', ' ')}</td>
                      <td data-label={t('用途')}>{settings.uses.find((u) => u.id === r.use)?.label ?? r.use}</td>
                      <td data-label={t('模型')}>{r.model}</td>
                      <td className="num" data-label={t('輸入')}>{nf.format(r.input_tokens)}</td>
                      <td className="num" data-label={t('輸出')}>{nf.format(r.output_tokens)}</td>
                      <td className="num" data-label={t('耗時')}>{(r.ms / 1000).toFixed(1)}s</td>
                      <td data-label={t('結果')} className={r.ok ? 'ok' : 'err'}>
                        {r.ok ? t('成功') : t.tr(ERROR_LABEL[r.error_kind ?? ''] ?? r.error_kind ?? msg('失敗'))}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </>
      )}
    </div>
  )

  const subTabItems: EnvSubTabItem<'form' | 'book'>[] = [
    { id: 'form', label: t('設定'), panelId: 'api-form', content: formContent },
    { id: 'book', label: t('帳本'), panelId: 'api-book', content: ledgerContent },
  ]

  return (
    <EnvSubTabs<'form' | 'book'>
      ariaLabel={t('Claude API 分類')}
      value={subTab}
      onChange={setSubTab}
      items={subTabItems}
    />
  )
}
