import { useEffect, useId, useMemo, useRef, useState } from 'react'
import {
  CREATABLE_KINDS,
  type BotKind,
  type ModelInfo,
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
import type { ShareProfile } from '../api/types'
import { ShareProfileField } from './ShareProfileField'
import { KindTag } from './KindTag'
import { isApiKind } from './kindMeta'
import { fetchOpenAiApiSettings } from '../api/openaiApi'
import { EMPTY_SHARE_FOLDER, shareCreateFields, shareFolderInput, shareProfileBlocked, type ShareFolderDraft } from '../lib/shareProfile'

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

function compareModelVersions(a: string, b: string): number {
  const av = (a.match(/\d+/g) ?? []).map(Number)
  const bv = (b.match(/\d+/g) ?? []).map(Number)
  for (let i = 0; i < Math.max(av.length, bv.length); i++) {
    const delta = (av[i] ?? -1) - (bv[i] ?? -1)
    if (delta !== 0) return delta
  }
  return 0
}

function pickLatestRoleModel(family: RegExp, list: readonly ModelInfo[]): ModelInfo | null {
  let best: ModelInfo | null = null
  for (const model of list) {
    if (model.id.includes('[1m]') || (!family.test(model.id) && !family.test(model.display_name))) continue
    if (!best || compareModelVersions(model.id, best.id) > 0) best = model
  }
  return best
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

  const existingManager = useMemo(() => {
    return bots.find((b) => b.project_id === pid && b.role === 'manager' && !b.parent_bot_id)
  }, [bots, pid])

  const [role, setRole] = useState<ExpertRoleChoice>('executor')

  const [track, setTrack] = useState('')

  const [kind, setKind] = useState<BotKind>('claude')
  const [shareProfile, setShareProfile] = useState<ShareProfile | null>(null)
  const [shareFolder, setShareFolder] = useState<ShareFolderDraft>(EMPTY_SHARE_FOLDER)
  const [model, setModel] = useState<string | null>(null)
  const [effort, setEffort] = useState<string | null>(null)
  const [fast, setFast] = useState(false)
  const [identity, setIdentity] = useState('')
  const [persona, setPersona] = useState('')

  const [customizeRuntime, setCustomizeRuntime] = useState(false)
  const [runtimeDraft, setRuntimeDraft] = useState<RuntimeSettings>({})

  const [managerUse, setManagerUse] = useState(true)

  const [name, setName] = useState(() => computeDefaultName('executor', '', pid, bots))
  const nameTouched = useRef(false)
  const effortTouched = useRef(false)
  const modelTouched = useRef(false)
  const [busy, setBusy] = useState(false)
  const [errorMsg, setErrorMsg] = useState<string | null>(null)
  const [openaiState, setOpenaiState] = useState<{ ready: boolean } | null>(null)
  useEffect(() => {
    let alive = true
    fetchOpenAiApiSettings()
      .then((s) => { if (alive) setOpenaiState({ ready: Boolean(s?.enabled && s.key_present) }) })
      .catch(() => { if (alive) setOpenaiState({ ready: false }) })
    return () => { alive = false }
  }, [])
  const openaiReady = openaiState?.ready === true

  const nameRef = useRef<HTMLInputElement>(null)
  const nameLabelId = useId()
  const nameHintId = useId()

  useEffect(() => {
    for (const k of CREATABLE_KINDS) {
      void loadModels(k, host)
    }
  }, [host, loadModels])

  const modelsCache = useStore((s) => s.models)

  const pickRoleSuggested = (r: ExpertRoleChoice): { kind: BotKind; model: string | null; effort: string | null } => {
    if (r === 'executor') {
      const claudeKey = `claude@${host || 'local'}@`
      const list = modelsCache[claudeKey] ?? []
      const found = pickLatestRoleModel(/haiku/i, list)
      return { kind: 'claude', model: found?.id ?? 'haiku', effort: 'low' }
    }
    if (r === 'manager') {
      const claudeKey = `claude@${host || 'local'}@`
      const list = modelsCache[claudeKey] ?? []
      const found = pickLatestRoleModel(/opus/i, list)
      return { kind: 'claude', model: found ? found.id : 'opus', effort: 'high' }
    }
    if (r === 'advisor') {
      const codexKey = `codex@${host || 'local'}@`
      const list = modelsCache[codexKey] ?? []
      const found = pickLatestRoleModel(/astra/i, list)
      return { kind: 'codex', model: found ? found.id : null, effort: 'high' }
    }
    if (r === 'specialist') {
      const claudeKey = `claude@${host || 'local'}@`
      const claudeList = modelsCache[claudeKey] ?? []
      const foundSonnet = pickLatestRoleModel(/sonnet/i, claudeList)
      if (foundSonnet) {
        return { kind: 'claude', model: foundSonnet.id, effort: 'medium' }
      }
      const codexKey = `codex@${host || 'local'}@`
      const codexList = modelsCache[codexKey] ?? []
      const foundSol = pickLatestRoleModel(/sol/i, codexList)
      if (foundSol) {
        return { kind: 'codex', model: foundSol.id, effort: 'medium' }
      }
      return { kind: 'claude', model: 'sonnet', effort: 'medium' }
    }
    return { kind: 'claude', model: null, effort: null }
  }

  const onPickRole = (r: ExpertRoleChoice) => {
    if (r === 'manager' && existingManager) return
    setRole(r)
    setErrorMsg(null)
    setCustomizeRuntime(false)
    setRuntimeDraft({})
    effortTouched.current = false
    modelTouched.current = false

    const sugg = pickRoleSuggested(r)
    setKind(sugg.kind)
    setModel(sugg.model)
    setEffort(null)

    if (!nameTouched.current) {
      setName(computeDefaultName(r, track, pid, bots))
    }
  }

  useEffect(() => {
    if (!modelTouched.current) {
      const sugg = pickRoleSuggested(role)
      setKind(sugg.kind)
      if (sugg.model) setModel(sugg.model)
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [modelsCache, host])

  useEffect(() => {
    if (role === 'specialist' && !nameTouched.current) {
      setName(computeDefaultName('specialist', track, pid, bots))
    }
  }, [role, track, pid, bots])

  const rolePolicy = project.role_policy?.[role] ?? {}

  const suggestedEffort = useMemo(() => {
    if (role === 'manager') return 'high'
    if (role === 'advisor') return 'high'
    if (role === 'specialist') return 'medium'
    if (role === 'executor') return 'low'
    return null
  }, [role])

  const nameOk = isValidBotName(name.trim())
  const apiKind = isApiKind(kind)
  const cliOk = kind === 'openai-api' ? openaiReady : apiKind || Boolean(tools[kind]?.installed)
  const trackOk = role !== 'specialist' || Boolean(track.trim())
  const rangeError = customizeRuntime ? firstRangeError(runtimeDraft) : null
  const shareOn = !apiKind && shareProfile !== null && !shareProfileBlocked(kind)
  const folderIn = shareOn ? shareFolderInput(shareFolder, name) : null
  const canSubmit = nameOk && cliOk && trackOk && !rangeError && !busy && !(folderIn && 'error' in folderIn)

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault()
    if (!canSubmit) return
    setBusy(true)
    setErrorMsg(null)

    const finalOverride = !apiKind && customizeRuntime && Object.keys(cleanSettings(runtimeDraft)).length > 0
      ? cleanSettings(runtimeDraft)
      : null

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
      fast: kind === 'codex' || apiKind ? fast : undefined,
      persona: persona.trim() || null,
      autostart: false,
      identity: apiKind ? null : identity || null,
      ...(apiKind ? {} : shareCreateFields(shareProfile, folderIn)),
    })

    if (!createdBotId) {
      setBusy(false)
      return
    }

    try {
      await patchBot(createdBotId, {
        role,
        track: role === 'specialist' ? track.trim() : null,
        manager_use: role === 'manager' ? true : managerUse,
        ...(apiKind ? {} : { runtime_override: finalOverride }),
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

  const roleDescriptions: Record<ExpertRoleChoice, string> = {
    manager: t('專案整體規劃、任務指派與進度管理（全專案僅一位）'),
    advisor: t('提供專業決策建議、架構評估與深度審查'),
    executor: t('依指派任務執行具體工作、回報成果'),
    specialist: t('專職負責特定領域或任務軌道（需指定 track）'),
  }

  const roleRecommendation = (r: ExpertRoleChoice): string | null => {
    if (r === 'manager') return 'Claude Opus (high)'
    if (r === 'advisor') return 'Codex Astra (high) / Claude API Opus (high)'
    if (r === 'executor') return 'Claude Haiku (low)'
    if (r === 'specialist') return 'Claude Sonnet (medium) / Codex Sol (medium)'
    return null
  }

  return (
    <form className="form new-expert-bot-form" onSubmit={handleSubmit}>
      {}
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

      {}
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

      {}
      <div className="field">
        <span>
          Kind
          {roleRecommendation(role) ? (
            <span className="field-note">{t('建議：{rec}', { rec: roleRecommendation(role) ?? '' })}</span>
          ) : null}
        </span>
        <div className="opt-group kinds" role="radiogroup" aria-label="kind">
          {CREATABLE_KINDS.map((k) => {
            const isApi = isApiKind(k)
            const openaiOff = k === 'openai-api' && !openaiReady
            const missing = openaiOff ? true : isApi ? false : !tools[k]?.installed
            const roleBlocked = isApi && role !== 'advisor'
            const title = roleBlocked ? (k === 'claude-api' ? t('claude-api 只能當顧問：不能接派工、不能用終端工具') : t('openai-api 只能當顧問：不能接派工、不能用終端工具')) : openaiOff ? t('先到 環境設定 → OpenAI API 啟用並設定金鑰') : missing ? t('{host} 尚未安裝 {kind}', { host: host === 'local' ? t('本機') : host, kind: k }) : k
            return (
              <span key={k} className="opt-wrap">
                <button
                  type="button"
                  role="radio"
                  aria-checked={kind === k}
                  className={`opt${kind === k ? ' on' : ''}`}
                  disabled={missing || roleBlocked}
                  title={title}
                  onClick={() => {
                    modelTouched.current = true
                    setKind(k)
                    setModel(null)
                    if (!effortTouched.current) {
                      setEffort(null)
                    }
                    setFast(false)
                    setIdentity('')
                    if (isApi) setShareProfile(null)
                  }}
                >
                  <KindTag kind={k} />
                  <span className="opt-label">{k}</span>
                </button>
                {roleBlocked ? (
                  <span className="kind-missing-reason">{t('限顧問')}</span>
                ) : missing ? (
                  <span className="kind-missing-reason">{openaiOff ? t('未設定') : t('未安裝')}</span>
                ) : null}
              </span>
            )
          })}
        </div>
      </div>

      <div>
        <ApiModelFields
          kind={kind}
          host={host}
          identity={identity || null}
          model={model}
          onModel={(m) => {
            modelTouched.current = true
            setModel(m)
          }}
          effort={effort}
          onEffort={(ef) => {
            effortTouched.current = true
            setEffort(ef)
          }}
          fast={fast}
          onFast={setFast}
          suggestedEffort={suggestedEffort}
        />
      </div>

      {apiKind ? null : <IdentityOptions kind={kind} host={host} value={identity} onChange={setIdentity} />}

      {}
      {apiKind ? null : <div className="field">
        <span>{t('Context 與保溫')}</span>
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
      </div>}

      {}
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

      {}
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

      {}
      <PersonaField value={persona} onChange={setPersona} collapsible />
      {apiKind ? null : <ShareProfileField kind={kind} host={host} value={shareProfile} onChange={setShareProfile} botName={name.trim()} folder={shareFolder} onFolder={setShareFolder} projectPath={project.path} />}

      {kind === 'openai-api' && !openaiReady ? <div className="hint warn" role="status">{t('先到 環境設定 → OpenAI API 啟用並設定金鑰')}</div> : null}
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
