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
import { JudgeLoadNotice, type JudgeLoad } from './JudgePanel'
import './claudeApiPanel.css'

const nf = new Intl.NumberFormat('en-US')

const ERROR_LABEL: Record<string, string> = {
  auth: 'key 被拒',
  rate_limited: '被限流',
  server: '伺服器錯誤',
  timeout: '逾時',
  network: '連不上',
  rejected: '請求被拒',
  bad_response: '回應不能用',
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
          setNote({ ok: true, text: '已儲存，立即生效。' })
        } else {
          setNote({ ok: false, text: `儲存失敗：${r.message}` })
        }
      })
      .catch((e: unknown) => setNote({ ok: false, text: `儲存失敗：${e instanceof Error ? e.message : String(e)}` }))
      .finally(() => setBusy(false))
  }

  const modelOptions = (current: string) => (settings.models.includes(current) ? settings.models : [...settings.models, current])

  return (
    <>
      <form
        className="form claude-api-panel"
        onSubmit={(e) => {
          e.preventDefault()
          if (!busy && dirty) save()
        }}
      >
        <p className="hint">
          一次性、不需要工具與 repo 的 LLM 工作（例如請 AGM 解析 changelog）改走 Anthropic API key，不開 Claude Code session，
          不吃訂閱用量；改按 API 計費。每個用途可以各自開關、選模型；失敗或沒 key 時，用途會退回原本的 Claude Code 路徑。
        </p>
        <label className="field">
          <span>
            Anthropic API key{' '}
            <em className={settings.key_present ? 'claude-api-key ok' : 'claude-api-key'}>{settings.key_present ? '已設定' : `未設定${settings.key_error ? `（${settings.key_error}）` : ''}`}</em>
          </span>
          <input
            type="password"
            value={token}
            autoComplete="off"
            spellCheck={false}
            placeholder={settings.key_present ? '留空＝不更換' : '貼上 key（sk-ant-…）'}
            onChange={(e) => setToken(e.target.value)}
          />
          <span className="hint">只寫進這台機器的 key 檔（權限 600），不進設定檔、不會再顯示。</span>
        </label>
        <label className="claude-api-switch">
          <input type="checkbox" checked={enabled} disabled={!willHaveKey && !enabled} onChange={(e) => setEnabled(e.target.checked)} />
          <span>總開關（關掉時下面各用途都不會呼叫 API；「更新框翻譯」沿用舊設定，不受總開關管）</span>
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
                  <span>啟用</span>
                </label>
                <div className="claude-api-use-fields">
                  <label className="field">
                    <span>模型</span>
                    <select value={model} onChange={(e) => patchUse(u.id, { model: e.target.value })}>
                      {modelOptions(model).map((m) => (
                        <option key={m} value={m}>
                          {m}
                          {m === u.default_model ? '（預設）' : ''}
                        </option>
                      ))}
                    </select>
                  </label>
                  <label className="field">
                    <span>思考強度</span>
                    <select value={effort} onChange={(e) => patchUse(u.id, { effort: e.target.value })}>
                      {settings.efforts.map((x) => (
                        <option key={x} value={x}>
                          {x}
                        </option>
                      ))}
                    </select>
                  </label>
                  <label className="field">
                    <span>每小時上限</span>
                    <input
                      type="number"
                      min={1}
                      max={1000}
                      value={perHour}
                      onChange={(e) => {
                        const n = Number.parseInt(e.target.value, 10)
                        if (Number.isFinite(n)) patchUse(u.id, { max_per_hour: n })
                      }}
                    />
                  </label>
                </div>
                {u.legacy_section ? <span className="hint">設定存在 [{u.legacy_section}]，不受總開關管。</span> : null}
              </fieldset>
            )
          })}
        </div>
        <div className="form-actions">
          {note ? <span className={note.ok ? 'claude-api-note ok' : 'claude-api-note err'} role="status">{note.text}</span> : null}
          <button type="submit" className="btn primary" disabled={busy || !dirty}>
            儲存
          </button>
        </div>
      </form>
      <div className="claude-api-ledger">
        <h4>
          用量帳本{' '}
          <button type="button" className="btn" onClick={loadLedger}>
            重新整理
          </button>
        </h4>
        {ledger === null ? (
          <p className="hint">讀不到帳本。</p>
        ) : (
          <>
            <p className="hint" data-testid="claude-api-totals">
              自 daemon 啟動起共 {ledger.totals.calls} 次（成功 {ledger.totals.ok}、失敗 {ledger.totals.errors}），輸入 {nf.format(ledger.totals.input_tokens)}、輸出{' '}
              {nf.format(ledger.totals.output_tokens)} tokens。這些工作沒有走訂閱額度。
            </p>
            {ledger.rows.length === 0 ? (
              <p className="hint">還沒有呼叫記錄。</p>
            ) : (
              <div className="claude-api-ledger-scroll">
                <table>
                  <thead>
                    <tr>
                      <th>時間</th>
                      <th>用途</th>
                      <th>模型</th>
                      <th>輸入</th>
                      <th>輸出</th>
                      <th>耗時</th>
                      <th>結果</th>
                    </tr>
                  </thead>
                  <tbody>
                    {ledger.rows.map((r, i) => (
                      <tr key={`${r.at}-${i}`}>
                        <td data-label="時間">{r.at.slice(5, 19).replace('T', ' ')}</td>
                        <td data-label="用途">{settings.uses.find((u) => u.id === r.use)?.label ?? r.use}</td>
                        <td data-label="模型">{r.model}</td>
                        <td data-label="輸入">{nf.format(r.input_tokens)}</td>
                        <td data-label="輸出">{nf.format(r.output_tokens)}</td>
                        <td data-label="耗時">{(r.ms / 1000).toFixed(1)}s</td>
                        <td data-label="結果" className={r.ok ? 'ok' : 'err'}>{r.ok ? '成功' : (ERROR_LABEL[r.error_kind ?? ''] ?? r.error_kind ?? '失敗')}</td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            )}
          </>
        )}
      </div>
    </>
  )
}
