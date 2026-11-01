import { useEffect, useState } from 'react'
import { BOT_KINDS, agyModelAllowed, type Bot, type BotKind, type ExecutorPoolEntry, type ModelInfo, type Project } from '../api/types'
import { useT } from '../lib/i18n'
import {
  POOL_CONTEXT_MIN,
  POOL_MAX_COUNT,
  POOL_MAX_ENTRIES,
  POOL_SUGGESTIONS,
  contextCap,
  formatContext,
  isDuplicate,
  makePoolId,
  resolveSuggestion,
} from '../lib/executorPool'
import { modelsKey } from '../store/modelsCache'
import { useStore } from '../store/store'
import { useSavePart } from './projectSaveGroup'

/** 專家模式的執行者池：條目列表、新增表單、預設建議（不自動加）。 */
export function ExecutorPoolEditor({ project, bots }: { project: Project; bots: Bot[] }) {
  const t = useT()
  const host = project.host || 'local'
  const loadModels = useStore((s) => s.loadModels)
  const modelsCache = useStore((s) => s.models)
  const patchProject = useStore((s) => s.patchProject)

  // 一次抓四種 kind 的模型清單：建議要用，新增表單也要用。
  useEffect(() => {
    for (const k of BOT_KINDS) void loadModels(k, host)
  }, [host, loadModels])

  const listFor = (kind: BotKind): ModelInfo[] => {
    const raw = modelsCache[modelsKey(kind, host)]
    const list = Array.isArray(raw) ? raw : []
    return kind === 'agy' ? list.filter((m) => agyModelAllowed(m.id)) : list
  }

  const [draft, setDraft] = useState<ExecutorPoolEntry[] | null>(null)
  const [saving, setSaving] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [showSuggest, setShowSuggest] = useState(false)
  const [picked, setPicked] = useState<Record<number, boolean>>({})
  const [showAdd, setShowAdd] = useState(false)

  const pool: ExecutorPoolEntry[] = draft ?? project.executor_pool ?? []
  const group = useSavePart('pool', { label: t('執行者池'), dirty: draft !== null, valid: true }, {
    patch: () => (draft === null ? {} : { executor_pool: draft }),
    reset: () => {
      setDraft(null)
      setError(null)
    },
  })
  const usedBy = (id: string) => bots.filter((b) => b.role === 'executor' && b.pool_entry === id).length

  const commit = (next: ExecutorPoolEntry[]) => {
    setDraft(next)
    setError(null)
  }

  const changeCount = (id: string, raw: string) => {
    const n = parseInt(raw, 10)
    const count = Number.isNaN(n) ? 1 : Math.max(1, Math.min(POOL_MAX_COUNT, n))
    commit(pool.map((e) => (e.id === id ? { ...e, count } : e)))
  }

  const removeEntry = (id: string) => commit(pool.filter((e) => e.id !== id))

  /** 新增一筆（已通過驗證的內容）；回傳錯誤訊息或 null。 */
  const addEntry = (cand: Omit<ExecutorPoolEntry, 'id'>, list: ExecutorPoolEntry[]): string | null => {
    if (list.length >= POOL_MAX_ENTRIES) return t('執行者池最多 {n} 條', { n: POOL_MAX_ENTRIES })
    if (isDuplicate(list, cand)) return t('已有相同的條目（種類、模型、強度、context 都一樣）')
    const ids = new Set(list.map((e) => e.id))
    list.push({ ...cand, id: makePoolId(cand, ids) })
    return null
  }

  // —— 新增表單 ——
  const kinds = BOT_KINDS
  const [fKind, setFKind] = useState<BotKind>('claude')
  const [fModel, setFModel] = useState('')
  const [fEffort, setFEffort] = useState('')
  const [fUseDefault, setFUseDefault] = useState(true)
  const [fContext, setFContext] = useState(POOL_CONTEXT_MIN)
  const [fCount, setFCount] = useState(1)
  const fList = listFor(fKind)
  const fModelInfo = fList.find((m) => m.id === fModel) ?? null
  const fCap = fModelInfo ? contextCap(fKind, fModelInfo, fList) : 200000
  const fCtxSupported = fKind !== 'agy'

  const changeKind = (k: BotKind) => {
    setFKind(k)
    setFModel('')
    setFEffort('')
    setFUseDefault(true)
  }
  const changeModel = (id: string) => {
    setFModel(id)
    setFEffort('')
    const info = listFor(fKind).find((m) => m.id === id)
    if (info) setFContext(Math.min(Math.max(POOL_CONTEXT_MIN, fContext), contextCap(fKind, info, listFor(fKind))))
  }

  const submitAdd = () => {
    if (!fModelInfo) {
      setError(t('請先選模型'))
      return
    }
    const cand: Omit<ExecutorPoolEntry, 'id'> = {
      kind: fKind,
      model: fModelInfo.id,
      effort: fEffort === '' ? null : fEffort,
      context_tokens: fCtxSupported && !fUseDefault ? fContext : null,
      count: fCount,
    }
    const next = [...pool]
    const err = addEntry(cand, next)
    if (err) {
      setError(err)
      return
    }
    commit(next)
    setShowAdd(false)
  }

  // —— 預設建議 ——
  const suggestions = POOL_SUGGESTIONS.map((s, i) => ({ i, s, resolved: resolveSuggestion(s, listFor(s.kind)) })).filter(
    (x): x is { i: number; s: (typeof POOL_SUGGESTIONS)[number]; resolved: Omit<ExecutorPoolEntry, 'id'> } => x.resolved !== null,
  )

  const addPicked = () => {
    const next = [...pool]
    let firstErr: string | null = null
    for (const { i, resolved } of suggestions) {
      if (!picked[i]) continue
      const err = addEntry(resolved, next)
      if (err && !firstErr) firstErr = err
    }
    commit(next)
    setPicked({})
    if (firstErr) setError(firstErr)
    else setShowSuggest(false)
  }

  const save = async () => {
    if (draft === null) return
    setSaving(true)
    const ok = await patchProject(project.id, { executor_pool: draft })
    setSaving(false)
    if (ok) setDraft(null)
    else setError(t('儲存失敗，請再試一次'))
  }

  const ctxLabel = (e: ExecutorPoolEntry) => {
    if (e.kind === 'agy') return t('不適用')
    return e.context_tokens === null ? t('模型預設') : formatContext(e.context_tokens)
  }

  return (
    <div className="project-executor-pool-section" data-testid="executor-pool">
      <h4>{t('執行者池')}</h4>
      <p className="hint">{t('管理者增聘執行者時只能用這裡的條目；條目決定種類、模型、強度與 context，數量是可增聘的上限。沒有條目＝不能增聘。')}</p>

      {pool.length === 0 ? <p className="hint">{t('還沒有條目')}</p> : null}
      <ul className="executor-pool-list">
        {pool.map((e) => (
          <li key={e.id} className="executor-pool-entry" data-pool-id={e.id}>
            <span className="executor-pool-kind">{e.kind}</span>
            <span className="executor-pool-model" title={e.model || undefined}>
              {e.model || t('任何模型（舊資料）')}
            </span>
            <span className="executor-pool-effort">{e.effort ?? t('預設強度')}</span>
            <span className="executor-pool-ctx">{ctxLabel(e)}</span>
            <span className="executor-pool-usage" aria-label={t('已用／上限')}>
              {usedBy(e.id)}/{e.count}
            </span>
            <input
              type="number"
              min={1}
              max={POOL_MAX_COUNT}
              value={e.count}
              aria-label={t('上限')}
              onChange={(ev) => changeCount(e.id, ev.target.value)}
              className="executor-pool-input"
            />
            <button type="button" className="btn executor-pool-remove" onClick={() => removeEntry(e.id)}>
              {t('刪除')}
            </button>
          </li>
        ))}
      </ul>

      <div className="executor-pool-actions">
        <button type="button" className="btn" aria-expanded={showSuggest} onClick={() => setShowSuggest((v) => !v)}>
          {t('加入建議')}
        </button>
        <button type="button" className="btn" aria-expanded={showAdd} onClick={() => setShowAdd((v) => !v)}>
          {t('新增條目')}
        </button>
      </div>

      {showSuggest ? (
        <div className="executor-pool-suggest" data-testid="executor-pool-suggest">
          <p className="hint">{t('勾選後按「加入勾選的建議」才會加入；模型取自目前可用的最新版，查不到的家族不列。')}</p>
          {suggestions.length === 0 ? <p className="hint">{t('目前讀不到模型清單，沒有可加入的建議。')}</p> : null}
          {suggestions.map(({ i, resolved }) => {
            const exists = isDuplicate(pool, resolved)
            return (
              <label key={i} className="executor-pool-suggest-row">
                <input
                  type="checkbox"
                  checked={!!picked[i]}
                  disabled={exists}
                  onChange={(ev) => setPicked((p) => ({ ...p, [i]: ev.target.checked }))}
                />
                <span>
                  {resolved.kind} · {resolved.model} · {resolved.effort ?? t('預設強度')} ·{' '}
                  {resolved.context_tokens === null ? (resolved.kind === 'agy' ? t('不適用') : t('模型預設')) : formatContext(resolved.context_tokens)} ·{' '}
                  ×{resolved.count}
                  {exists ? ` (${t('已在池中')})` : ''}
                </span>
              </label>
            )
          })}
          <div className="executor-pool-actions">
            <button type="button" className="btn" disabled={!suggestions.some(({ i, resolved }) => picked[i] && !isDuplicate(pool, resolved))} onClick={addPicked}>
              {t('加入勾選的建議')}
            </button>
          </div>
        </div>
      ) : null}

      {showAdd ? (
        <div className="executor-pool-form" data-testid="executor-pool-form">
          <label>
            {t('種類')}{' '}
            <select value={fKind} onChange={(ev) => changeKind(ev.target.value as BotKind)}>
              {kinds.map((k) => (
                <option key={k} value={k}>
                  {k}
                </option>
              ))}
            </select>
          </label>
          <label>
            {t('模型')}{' '}
            <select value={fModel} onChange={(ev) => changeModel(ev.target.value)} disabled={fList.length === 0}>
              <option value="">{fList.length === 0 ? t('無法取得模型清單') : t('請選擇')}</option>
              {fList.map((m) => (
                <option key={m.id} value={m.id}>
                  {m.id}
                </option>
              ))}
            </select>
          </label>
          <label>
            {t('強度')}{' '}
            <select value={fEffort} onChange={(ev) => setFEffort(ev.target.value)} disabled={!fModelInfo || fModelInfo.efforts.length === 0}>
              <option value="">{t('預設強度')}</option>
              {(fModelInfo?.efforts ?? []).map((ef) => (
                <option key={ef} value={ef}>
                  {ef}
                </option>
              ))}
            </select>
          </label>
          <fieldset className="executor-pool-ctx-field" disabled={!fCtxSupported}>
            <legend>{t('context')}</legend>
            {!fCtxSupported ? <p className="hint">{t('agy 不支援 context 設定，固定用模型預設。')}</p> : null}
            <label>
              <input type="checkbox" checked={fUseDefault} onChange={(ev) => setFUseDefault(ev.target.checked)} /> {t('用模型預設')}
            </label>
            {fCtxSupported && !fUseDefault ? (
              <label className="executor-pool-ctx-range">
                <input
                  type="range"
                  min={POOL_CONTEXT_MIN}
                  max={fCap}
                  step={1000}
                  value={Math.min(fContext, fCap)}
                  onChange={(ev) => setFContext(Number(ev.target.value))}
                  aria-label={t('context 上限（tokens）')}
                />
                <span>{formatContext(Math.min(fContext, fCap))}</span>
              </label>
            ) : null}
          </fieldset>
          <label>
            {t('數量')}{' '}
            <input
              type="number"
              min={1}
              max={POOL_MAX_COUNT}
              value={fCount}
              onChange={(ev) => setFCount(Math.max(1, Math.min(POOL_MAX_COUNT, parseInt(ev.target.value, 10) || 1)))}
              className="executor-pool-input"
            />
          </label>
          <div className="executor-pool-actions">
            <button type="button" className="btn" disabled={!fModelInfo} onClick={submitAdd}>
              {t('加入條目')}
            </button>
          </div>
        </div>
      ) : null}

      {error ? (
        <p className="executor-pool-error" role="alert">
          {error}
        </p>
      ) : null}

      {group.grouped ? null : (
        <div className="project-executor-pool-actions">
          <button type="button" className="btn" disabled={saving || draft === null} onClick={() => void save()}>
            {saving ? t('儲存中…') : t('儲存執行者池')}
          </button>
        </div>
      )}
    </div>
  )
}
