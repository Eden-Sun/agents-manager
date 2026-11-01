import { useEffect, useId, useMemo, useRef, useState } from 'react'
import {
  BOT_KINDS,
  type BotKind,
  type ExecutorPoolEntry,
  type Project,
  type RuntimeSettings,
} from '../api/types'
import { useT } from '../lib/i18n'
import {
  cleanSettings,
  formatContext,
  firstRangeError,
} from '../lib/runtimePolicy'
import { BOT_NAME_HINT, isValidBotName } from '../lib/botName'
import { modelMatches } from '../lib/modelMatches'
import { projectHostName, toolsOfHost, useStore } from '../store/store'
import { ApiModelFields } from './ModelPicker'
import { IdentityOptions, PersonaField } from './BotSettingsPanel'
import { RuntimeSettingsFields } from './RuntimeSettingsFields'
import { pickSuggestedModel } from '../lib/executorPool'

export type ExpertRoleChoice = 'manager' | 'advisor' | 'executor' | 'specialist'

export interface NewExpertBotFormProps {
  project: Project
  onDone: () => void
  onCreated?: (botId: string) => void
}

function computeDefaultName(r: ExpertRoleChoice, trk: string, pid: string, bots: { project_id: string; name: string }[]): string {
  const taken = new Set(bots.filter((b) => b.project_id === pid).map((b) => b.name))
  if (r === 'manager') return 'manager'
  if (r === 'advisor') {
    let n = 1
    while (taken.has(`advisor-${n}`)) n++
    return `advisor-${n}`
  }
  if (r === 'executor') {
    let n = 1
    while (taken.has(`exec-${n}`)) n++
    return `exec-${n}`
  }
  if (r === 'specialist') {
    return trk.trim()
  }
  return ''
}

export function NewExpertBotForm({ project, onDone, onCreated }: NewExpertBotFormProps) {
  const t = useT()
  const pid = project.id
  const bots = useStore((s) => s.bots)
  const host = useStore((s) => projectHostName(s, pid))
  const tools = useStore((s) => toolsOfHost(s, host))
  const addBot = useStore((s) => s.addBot)
  const patchBot = useStore((s) => s.patchBot)
  const startBot = useStore((s) => s.startBot)
  const loadModels = useStore((s) => s.loadModels)

  // 1. 角色
  const existingManager = useMemo(() => {
    return bots.find((b) => b.project_id === pid && b.role === 'manager' && !b.parent_bot_id)
  }, [bots, pid])

  const [role, setRole] = useState<ExpertRoleChoice>(() => (existingManager ? 'specialist' : 'manager'))

  // 專家 track
  const [track, setTrack] = useState('')

  // 執行者池條目（'custom' 或 entry.id）
  const poolEntries = project.executor_pool ?? []
  const [selectedPoolEntryId, setSelectedPoolEntryId] = useState<string>(() => {
    return poolEntries.length > 0 ? poolEntries[0].id : 'custom'
  })

  // kind / model / effort / fast
  const [kind, setKind] = useState<BotKind>('claude')
  const [model, setModel] = useState<string | null>(null)
  const [effort, setEffort] = useState<string | null>(null)
  const [fast, setFast] = useState(false)
  const [identity, setIdentity] = useState('')
  const [persona, setPersona] = useState('')

  // context / 保溫自訂
  const [customizeRuntime, setCustomizeRuntime] = useState(false)
  const [runtimeDraft, setRuntimeDraft] = useState<RuntimeSettings>({})

  // 可被管理者使用
  const [managerUse, setManagerUse] = useState(true)

  // 名稱
  const [name, setName] = useState(() => computeDefaultName(existingManager ? 'specialist' : 'manager', '', pid, bots))
  const nameTouched = useRef(false)
  const effortTouched = useRef(false)
  const [busy, setBusy] = useState(false)
  const [errorMsg, setErrorMsg] = useState<string | null>(null)

  const nameRef = useRef<HTMLInputElement>(null)
  const nameLabelId = useId()
  const nameHintId = useId()

  // 預載各 kind 模型清單
  useEffect(() => {
    for (const k of BOT_KINDS) {
      void loadModels(k, host)
    }
  }, [host, loadModels])

  const modelsCache = useStore((s) => s.models)

  // 挑選建議模型
  const pickRoleSuggested = (r: ExpertRoleChoice): { kind: BotKind; model: string | null; effort: string | null } => {
    if (r === 'manager') {
      const claudeKey = `claude@${host || 'local'}@`
      const list = modelsCache[claudeKey] ?? []
      const found = pickSuggestedModel({ kind: 'claude', family: /opus/i, effort: 'high', context_tokens: null, count: 1 }, list)
      return { kind: 'claude', model: found ? found.id : null, effort: 'high' }
    }
    if (r === 'advisor') {
      const codexKey = `codex@${host || 'local'}@`
      const list = modelsCache[codexKey] ?? []
      const found = pickSuggestedModel({ kind: 'codex', family: /astra/i, effort: 'high', context_tokens: null, count: 1 }, list)
      return { kind: 'codex', model: found ? found.id : null, effort: 'high' }
    }
    if (r === 'specialist') {
      // claude sonnet medium 或 codex 最新 sol medium
      const claudeKey = `claude@${host || 'local'}@`
      const claudeList = modelsCache[claudeKey] ?? []
      const foundSonnet = pickSuggestedModel({ kind: 'claude', family: /sonnet/i, effort: 'medium', context_tokens: null, count: 1 }, claudeList)
      if (foundSonnet) {
        return { kind: 'claude', model: foundSonnet.id, effort: 'medium' }
      }
      const codexKey = `codex@${host || 'local'}@`
      const codexList = modelsCache[codexKey] ?? []
      const foundSol = pickSuggestedModel({ kind: 'codex', family: /sol/i, effort: 'medium', context_tokens: null, count: 1 }, codexList)
      if (foundSol) {
        return { kind: 'codex', model: foundSol.id, effort: 'medium' }
      }
      return { kind: 'claude', model: null, effort: 'medium' }
    }
    // executor default custom
    return { kind: 'claude', model: null, effort: null }
  }

  // 套用池條目
  const applyPoolEntry = (entry: ExecutorPoolEntry) => {
    setKind(entry.kind)
    setModel(entry.model || null)
    setEffort(entry.effort)
    setFast(false)
  }

  // 當 role 切換時
  const onPickRole = (r: ExpertRoleChoice) => {
    if (r === 'manager' && existingManager) return
    setRole(r)
    setErrorMsg(null)
    setCustomizeRuntime(false)
    setRuntimeDraft({})
    effortTouched.current = false

    if (r === 'executor') {
      if (poolEntries.length > 0) {
        setSelectedPoolEntryId(poolEntries[0].id)
        applyPoolEntry(poolEntries[0])
      } else {
        setSelectedPoolEntryId('custom')
        const sugg = pickRoleSuggested(r)
        setKind(sugg.kind)
        setModel(sugg.model)
        setEffort(null)
      }
    } else {
      setSelectedPoolEntryId('custom')
      const sugg = pickRoleSuggested(r)
      setKind(sugg.kind)
      setModel(sugg.model)
      setEffort(null)
    }

    if (!nameTouched.current) {
      setName(computeDefaultName(r, track, pid, bots))
    }
  }

  // 選中池條目變更
  const onSelectPoolEntry = (id: string) => {
    setSelectedPoolEntryId(id)
    if (id === 'custom') {
      effortTouched.current = false
      setEffort(null)
    } else {
      const found = poolEntries.find((e) => e.id === id)
      if (found) {
        applyPoolEntry(found)
      }
    }
  }

  // 專家 track 切換時更新預設名稱
  useEffect(() => {
    if (role === 'specialist' && !nameTouched.current) {
      setName(computeDefaultName('specialist', track, pid, bots))
    }
  }, [role, track, pid, bots])

  // 角色預設政策
  const rolePolicy = project.role_policy?.[role] ?? {}

  const selectedPool = role === 'executor' && selectedPoolEntryId !== 'custom' ? poolEntries.find((e) => e.id === selectedPoolEntryId) : null
  const isPoolLocked = Boolean(selectedPool)

  const suggestedEffort = useMemo(() => {
    if (role === 'manager') return 'high'
    if (role === 'advisor') return 'high'
    if (role === 'specialist') return 'medium'
    if (role === 'executor') return selectedPool?.effort ?? null
    return null
  }, [role, selectedPool])

  // 驗證
  const nameOk = isValidBotName(name.trim())
  const cliOk = Boolean(tools[kind]?.installed)
  const trackOk = role !== 'specialist' || Boolean(track.trim())
  const rangeError = customizeRuntime ? firstRangeError(runtimeDraft) : null
  const canSubmit = nameOk && cliOk && trackOk && !rangeError && !busy

  // 送出
  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault()
    if (!canSubmit) return
    setBusy(true)
    setErrorMsg(null)

    const finalOverride = customizeRuntime && Object.keys(cleanSettings(runtimeDraft)).length > 0
      ? cleanSettings(runtimeDraft)
      : null

    // 模型目前可用的強度清單與預設值
    const kindKey = `${kind}@${host || 'local'}@${identity || ''}`
    const modelsList = modelsCache[kindKey] ?? []
    const defaultM = modelsList.find((m) => m.is_default) ?? modelsList[0]
    const curM = (model && (modelsList.find((m) => m.id === model) ?? modelsList.find((m) => modelMatches(m.id, model)))) || defaultM
    const curEfforts = curM?.efforts ?? []
    const fallbackEffort = (suggestedEffort && curEfforts.includes(suggestedEffort))
      ? suggestedEffort
      : (curM?.account_default_effort ?? curM?.default_effort ?? null)
    const effectiveEffort = effortTouched.current ? effort : (effort ?? fallbackEffort)

    const createdBotId = await addBot(pid, {
      name: name.trim(),
      kind,
      model,
      effort: effectiveEffort,
      fast: kind === 'codex' ? fast : undefined,
      persona: persona.trim() || null,
      autostart: false,
      identity: identity || null,
      auto_approve: true,
    })

    if (!createdBotId) {
      setBusy(false)
      return
    }

    // PATCH /api/bots/{id} with role, track, manager_use, runtime_override
    try {
      await patchBot(createdBotId, {
        role,
        track: role === 'specialist' ? track.trim() : null,
        manager_use: role === 'manager' ? true : managerUse,
        runtime_override: finalOverride,
      })
      setBusy(false)
      onCreated?.(createdBotId)
      onDone()
      await startBot(createdBotId)
    } catch (err: unknown) {
      setBusy(false)
      const msg = err instanceof Error ? err.message : String(err)
      setErrorMsg(t('角色設定失敗，Bot 已保留，請於設定選單中補設：{err}', { err: msg }))
    }
  }

  // 角色說明文字
  const roleDescriptions: Record<ExpertRoleChoice, string> = {
    manager: t('專案整體規劃、任務指派與進度管理（全專案僅一位）'),
    advisor: t('提供專業決策建議、架構評估與深度審查'),
    executor: t('依指派任務執行具體工作、回報成果'),
    specialist: t('專職負責特定領域或任務軌道（需指定 track）'),
  }

  // 角色建議提示
  const roleRecommendation = (r: ExpertRoleChoice): string | null => {
    if (r === 'manager') return 'Claude Opus (high)'
    if (r === 'advisor') return 'Codex Astra (high)'
    if (r === 'specialist') return 'Claude Sonnet (medium) / Codex Sol (medium)'
    return null
  }

  return (
    <form className="form new-expert-bot-form" onSubmit={handleSubmit}>
      {/* 1. 角色選擇 */}
      <div className="field">
        <span>{t('角色')}</span>
        <div className="expert-role-picker" role="radiogroup" aria-label={t('角色')}>
          {(['manager', 'advisor', 'executor', 'specialist'] as const).map((r) => {
            const isMgr = r === 'manager'
            const disabled = isMgr && Boolean(existingManager)
            const selected = role === r
            const label = r === 'manager' ? t('管理者') : r === 'advisor' ? t('顧問') : r === 'executor' ? t('執行者') : t('專家')
            return (
              <label
                key={r}
                className={`expert-role-card${selected ? ' on' : ''}${disabled ? ' disabled' : ''}`}
              >
                <div className="expert-role-card-top">
                  <input
                    type="radio"
                    name="expert-role"
                    checked={selected}
                    disabled={disabled}
                    onChange={() => onPickRole(r)}
                  />
                  <strong className="expert-role-card-title">{label}</strong>
                  {disabled && isMgr && existingManager ? (
                    <span className="expert-mgr-taken">
                      {t('已有管理者 {name}', { name: existingManager.name })}
                    </span>
                  ) : null}
                </div>
                <div className="expert-role-card-desc">{roleDescriptions[r]}</div>
              </label>
            )
          })}
        </div>
      </div>

      {/* 專家模式專屬：負責範圍 (track) */}
      {role === 'specialist' ? (
        <label className="field">
          <span>
            {t('負責範圍（track）')} <span className="field-required">*</span>
          </span>
          <input
            type="text"
            required
            value={track}
            placeholder={t('例如：billing、CI')}
            onChange={(e) => {
              const val = e.target.value
              setTrack(val)
              if (!nameTouched.current) {
                setName(val.trim())
              }
            }}
          />
          {!track.trim() ? <span className="hint warn">{t('專家的負責範圍（track）為必填')}</span> : null}
        </label>
      ) : null}

      {/* 執行者模式專屬：執行者池條目 */}
      {role === 'executor' ? (
        <div className="field">
          <span>{t('執行者池條目')}</span>
          {poolEntries.length === 0 ? (
            <div className="hint">{t('目前池是空的，直接走自訂設定。')}</div>
          ) : (
            <div className="opt-group" role="radiogroup" aria-label={t('執行者池條目')}>
              {poolEntries.map((e) => {
                const used = bots.filter((b) => b.role === 'executor' && b.pool_entry === e.id).length
                const ctxText = e.kind === 'agy' ? t('不適用') : e.context_tokens ? formatContext(e.context_tokens) : t('預設')
                return (
                  <button
                    key={e.id}
                    type="button"
                    role="radio"
                    aria-checked={selectedPoolEntryId === e.id}
                    className={`opt${selectedPoolEntryId === e.id ? ' on' : ''}`}
                    onClick={() => onSelectPoolEntry(e.id)}
                  >
                    {e.kind} · {e.model || t('任何模型')} · {e.effort ?? t('預設強度')} · {ctxText} · {used}/{e.count}
                  </button>
                )
              })}
              <button
                type="button"
                role="radio"
                aria-checked={selectedPoolEntryId === 'custom'}
                className={`opt${selectedPoolEntryId === 'custom' ? ' on' : ''}`}
                onClick={() => onSelectPoolEntry('custom')}
              >
                {t('自訂')}
              </button>
            </div>
          )}
        </div>
      ) : null}

      {/* 2. kind / 模型 / 強度 */}
      <div className="field">
        <span>
          kind
          {roleRecommendation(role) && !isPoolLocked ? (
            <span className="field-note">{t('建議：{rec}', { rec: roleRecommendation(role) ?? '' })}</span>
          ) : null}
        </span>
        <div className="opt-group kinds" role="radiogroup" aria-label="kind">
          {BOT_KINDS.map((k) => {
            const missing = !tools[k]?.installed
            return (
              <span key={k} className="opt-wrap">
                <button
                  type="button"
                  role="radio"
                  aria-checked={kind === k}
                  className={`opt${kind === k ? ' on' : ''}`}
                  disabled={missing || isPoolLocked}
                  onClick={() => {
                    if (isPoolLocked) return
                    setKind(k)
                    setModel(null)
                    if (!effortTouched.current) {
                      setEffort(null)
                    }
                    setFast(false)
                  }}
                >
                  <span className="opt-label">{k}</span>
                </button>
              </span>
            )
          })}
        </div>
      </div>

      <div className={isPoolLocked ? 'fields-locked' : undefined}>
        <ApiModelFields
          kind={kind}
          host={host}
          identity={identity || null}
          model={model}
          onModel={(m) => {
            if (!isPoolLocked) setModel(m)
          }}
          effort={effort}
          onEffort={(ef) => {
            if (!isPoolLocked) {
              effortTouched.current = true
              setEffort(ef)
            }
          }}
          fast={fast}
          onFast={(f) => {
            if (!isPoolLocked) setFast(f)
          }}
          suggestedEffort={suggestedEffort}
        />
      </div>

      <IdentityOptions kind={kind} host={host} value={identity} onChange={setIdentity} />

      {/* 3. context 與保溫 */}
      <div className="field">
        <span>{t('context 與保溫')}</span>
        <div className="expert-runtime-summary">
          <div className="expert-runtime-summary-row">
            <span>
              {t('預設政策（{role}）：', {
                role: role === 'manager' ? t('管理者') : role === 'advisor' ? t('顧問') : role === 'executor' ? t('執行者') : t('專家'),
              })}
            </span>
            <span className="expert-runtime-vals">
              context: {rolePolicy.context_tokens ? formatContext(rolePolicy.context_tokens) : t('跟預設')}
              {' · '}
              {kind === 'codex' ? t('永快取') : (
                rolePolicy.keep_warm === true ? t('保溫開') : rolePolicy.keep_warm === false ? t('保溫關') : t('保溫跟預設')
              )}
            </span>
            <button
              type="button"
              className="btn btn-sm"
              onClick={() => {
                setCustomizeRuntime(!customizeRuntime)
                if (!customizeRuntime && Object.keys(runtimeDraft).length === 0) {
                  setRuntimeDraft({ ...rolePolicy })
                }
              }}
            >
              {customizeRuntime ? t('收起自訂') : t('自訂')}
            </button>
          </div>
          {customizeRuntime ? (
            <div className="expert-runtime-editor">
              <RuntimeSettingsFields
                value={runtimeDraft}
                onChange={setRuntimeDraft}
                inheritLabel={t('跟角色')}
                idPrefix="new-expert-bot-rt"
                codexNote={kind === 'codex'}
              />
            </div>
          ) : null}
        </div>
      </div>

      {/* 4. 可被管理者使用 */}
      {role !== 'manager' ? (
        <label className="field row">
          <input
            type="checkbox"
            checked={managerUse}
            onChange={(e) => setManagerUse(e.target.checked)}
          />
          <span>{t('可被管理者使用')}</span>
        </label>
      ) : null}

      {/* 5. 名稱 */}
      <label className="field">
        <span id={nameLabelId}>{t('名稱')}</span>
        <input
          ref={nameRef}
          type="text"
          aria-labelledby={nameLabelId}
          aria-invalid={name && !nameOk ? true : undefined}
          aria-describedby={name && !nameOk ? nameHintId : undefined}
          value={name}
          spellCheck={false}
          onChange={(e) => {
            nameTouched.current = true
            setName(e.target.value)
          }}
        />
        {name && !nameOk ? <span id={nameHintId} className="hint">{t.tr(BOT_NAME_HINT)}</span> : null}
      </label>

      {/* 人設保留 */}
      <PersonaField value={persona} onChange={setPersona} collapsible />

      {errorMsg ? <div className="hint warn" role="alert">{errorMsg}</div> : null}

      <div className="form-actions">
        <button type="button" className="btn" onClick={onDone}>
          {t('取消')}
        </button>
        <button type="submit" className="btn primary" disabled={!canSubmit}>
          {busy ? t('建立中…') : t('新增並啟動')}
        </button>
      </div>
    </form>
  )
}
