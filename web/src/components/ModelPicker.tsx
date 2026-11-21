import { useEffect, useId, useLayoutEffect, useRef, useState } from 'react'
import { isApiKind } from './kindMeta'
import type { ReactNode } from 'react'
import { createPortal } from 'react-dom'
import type { BotKind, ModelInfo, ModelsMeta, PatchBotInput } from '../api/types'
import { CLAUDE_EFFORT_OPTIONS, CODEX_EFFORT_OPTIONS, EFFORT_OPTIONS, FAST_TIER, HIDDEN_MODELS, MODEL_OPTIONS, openAiApiEffortsFor, agyModelAllowed, canonicalModel, effortLabel } from '../api/types'
import { compareGptModels, formatGptModelLabel } from '../lib/gptModel'
import { buildAgyFamilyOrder, compareAgyModels, formatAgyEffortLabel, formatAgyModelLabel } from '../lib/agyModel'
import { agyEffortOf, agyGroupMembers, agyGroupOf, groupAgyModels, pickAgyVariant } from '../lib/agyModelGroup'
import { useMenuKeys } from '../hooks/useMenuKeys'
import { modelMatches } from '../lib/modelMatches'
import { modelSwitchPatch } from '../lib/modelSwitch'
import { ensureCurrentModel, latestModels } from '../lib/modelSeries'
import { runningFast } from '../lib/runtimeDrift'
import { t, useT } from '../lib/i18n'
import { useStore } from '../store/store'
import { lastKnownModels, modelsKey } from '../store/modelsCache'
import { KindTag } from './KindTag'
import './modelPicker.css'

function staticModels(kind: BotKind): ModelInfo[] {
  const efforts =
    kind === 'agy' ? [] : kind === 'grok' ? [...EFFORT_OPTIONS] : kind === 'codex' ? [...CODEX_EFFORT_OPTIONS] : [...CLAUDE_EFFORT_OPTIONS]
  return MODEL_OPTIONS[kind].map((id, i) => ({
    id,
    display_name: id,
    description: '',
    is_default: kind === 'openai-api' ? id.includes('astra') : i === 0,
    default_effort: kind === 'claude-api' ? 'high' : kind === 'openai-api' ? 'medium' : null,
    efforts: kind === 'openai-api' ? [...openAiApiEffortsFor(id)] : efforts,
    service_tiers: kind === 'claude-api' ? [{ id: 'fast', name: 'fast', description: t('speed=fast（單價 2 倍）') }] : [],
  }))
}

function apiModelList(kind: BotKind, cached: ModelInfo[] | null | undefined): ModelInfo[] {
  if (!Array.isArray(cached)) return []
  return kind === 'agy' ? cached.filter((m) => agyModelAllowed(m.id)) : cached
}

function visibleModels(models: ModelInfo[], selected: string | null): ModelInfo[] {
  return models.filter((m) => modelMatches(m.id, selected) || !HIDDEN_MODELS.includes(m.id))
}

function modelLabel(m: ModelInfo, kind?: BotKind): string {
  if (kind === 'codex' || kind === 'openai-api' || /^gpt-\d/i.test(m.id)) {
    const formatted = formatGptModelLabel(m.id)
    if (formatted !== m.id) return formatted
  }
  if (kind === 'agy' || /^gemini-\d/i.test(m.id)) {
    const formatted = formatAgyModelLabel(m.id)
    if (formatted !== m.id) return formatted
  }
  const name = m.display_name || m.id
  return name.replace(/^gpt-/i, '')
}

function sortModels(kind: BotKind, models: ModelInfo[]): ModelInfo[] {
  if (kind === 'codex' || kind === 'openai-api') {
    return models
      .map((m, i) => ({ m, i }))
      .sort((a, b) => compareGptModels(canonicalModel(kind, a.m.id), canonicalModel(kind, b.m.id)) || a.i - b.i)
      .map((x) => x.m)
  }
  if (kind === 'agy') {
    const familyOrder = buildAgyFamilyOrder(models.map((m) => m.id))
    return models
      .map((m, i) => ({ m, i }))
      .sort((a, b) => compareAgyModels(a.m.id, b.m.id, familyOrder) || a.i - b.i)
      .map((x) => x.m)
  }
  const want = MODEL_OPTIONS[kind] ?? []
  const rank = (id: string) => {
    const at = want.indexOf(canonicalModel(kind, id))
    return at < 0 ? want.length : at
  }
  return models
    .map((m, i) => ({ m, i }))
    .sort((a, b) => rank(a.m.id) - rank(b.m.id) || a.i - b.i)
    .map((x) => x.m)
}

const isFastTier = (id: string) => id === FAST_TIER || id === 'fast'

function fallbackWarning(
  t: ReturnType<typeof useT>,
  kind: BotKind,
  meta: ModelsMeta | undefined,
  usingLast: boolean,
  cachedNull: boolean,
  fromApi: boolean,
): string | null {
  if (fromApi && meta?.source === 'other-host') {
    const rawHost = meta.source_host || 'local'
    const sourceHost = rawHost === 'local' ? t('本機') : rawHost
    return t('此主機的模型清單暫時讀不到，沿用 {source_host} 的清單', { source_host: sourceHost })
  }
  if ((fromApi && meta?.stale) || (usingLast && cachedNull)) {
    return t('此主機的模型清單暫時讀不到，顯示的是上次取得的備用清單')
  }
  if ((fromApi && meta?.source === 'static' && kind !== 'claude') || !fromApi) {
    return t('此主機的模型清單暫時讀不到，顯示的是備用清單')
  }
  return null
}

export function ApiModelFields({
  kind,
  host,
  identity,
  model,
  onModel,
  effort,
  onEffort,
  fast,
  onFast,
  suggestedEffort,
}: {
  kind: BotKind
  host: string
  identity?: string | null
  model: string | null
  onModel: (v: string | null) => void
  effort: string | null
  onEffort: (v: string | null) => void
  fast: boolean
  onFast: (v: boolean) => void
  suggestedEffort?: string | null
}) {
  const t = useT()
  const key = `${kind}@${host || 'local'}@${identity || ''}`
  const cached = useStore((s) => s.models[key])
  const meta = useStore((s) => s.modelsMeta[key])
  const loadModels = useStore((s) => s.loadModels)
  useEffect(() => {
    if (cached == null) void loadModels(kind, host, identity)
  }, [cached, kind, host, identity, loadModels])

  const loading = cached === undefined
  const last = Array.isArray(cached) ? null : lastKnownModels(kind, modelsKey(kind, host, identity))
  const apiModels = apiModelList(kind, Array.isArray(cached) ? cached : last)
  const usingLast = last !== null && apiModels.length > 0
  const fromApi = apiModels.length > 0
  const rawModels = fromApi ? apiModels : staticModels(kind)
  const { models, missingIds } = ensureCurrentModel(rawModels, model, kind)
  const latest = latestModels(kind, visibleModels(models, model), model)
  const shownModels = sortModels(kind, latest.models)
  const defaultModel = models.find((m) => m.is_default) ?? models[0]
  const current = (model && (models.find((m) => m.id === model) ?? models.find((m) => modelMatches(m.id, model)))) || defaultModel
  const selectedModel = model ?? defaultModel?.id ?? null
  const agy = kind === 'agy'
  const agyGroups = agy ? groupAgyModels(shownModels) : []
  const selGroup = agy ? (agyGroupOf(agyGroups, selectedModel) ?? agyGroups.find((g) => g.isDefault) ?? agyGroups[0] ?? null) : null
  const selGroupEffort = agyEffortOf(selGroup, selectedModel)
  const selGroupAllMissing = selGroup ? agyGroupMembers(selGroup).every((v) => missingIds.has(v.id)) : false
  const efforts = agy ? (selGroup?.efforts ?? []) : (current?.efforts ?? [])
  const fallbackEffort = (suggestedEffort && efforts.includes(suggestedEffort)) ? suggestedEffort : effectiveDefaultEffort(current)
  const selectedEffort = agy ? selGroupEffort : (effort ?? fallbackEffort)
  const hasFast = current?.service_tiers.some((t) => isFastTier(t.id)) ?? false
  const warnText = fallbackWarning(t, kind, meta, usingLast, cached === null, fromApi)

  const effortUnsupported = fromApi && effort !== null && efforts.length > 0 && !efforts.includes(effort)
  const fastUnsupported = fromApi && fast && !hasFast

  const retiredTo = kind === 'agy' && model ? canonicalModel(kind, model) : null
  const retiredModel = retiredTo !== null && retiredTo !== model ? { from: model as string, to: retiredTo } : null

  const pickModel = (id: string | null) => {
    onModel(id)
    const next = (id && models.find((m) => m.id === id)) || defaultModel
    if (fromApi && fast && next && !next.service_tiers.some((t) => isFastTier(t.id))) onFast(false)
    if (effort !== null && next && next.efforts.length > 0 && !next.efforts.includes(effort)) {
      const fallback = (suggestedEffort && next.efforts.includes(suggestedEffort)) ? suggestedEffort : next.default_effort
      onEffort(fallback)
    }
  }

  return (
    <>
      <div className="field">
        <span>
          {t('模型')}
          {loading ? (
            <span className="field-note">{t('載入中…')}</span>
          ) : warnText ? (
            <span className="field-note warn">{warnText}</span>
          ) : null}
          {}
        </span>
        {}
        <div className="opt-group models" role="radiogroup" aria-label={t('模型')}>
          {agy ? agyGroups.map((g) => {
            const on = selGroup === g
            const members = agyGroupMembers(g)
            const allMissing = members.every((v) => missingIds.has(v.id))
            const allOld = members.every((v) => latest.oldIds.has(v.id))
            const target = pickAgyVariant(g, selGroupEffort)
            return (
              <button
                key={g.key}
                type="button"
                role="radio"
                aria-checked={on}
                className={`opt${on ? ' on' : ''}`}
                title={[target?.id ?? g.key, g.isDefault ? t('模型預設') : ''].filter(Boolean).join(' — ')}
                onClick={() => { if (target) pickModel(target.id) }}
              >
                {g.label}
                {allMissing ? <span className="field-note"> {t('不在清單')}</span> : allOld ? <span className="field-note"> {t('舊版')}</span> : null}
              </button>
            )
          }) : shownModels.map((m) => (
            <button
              key={m.id}
              type="button"
              role="radio"
              aria-checked={modelMatches(m.id, selectedModel)}
              className={`opt${modelMatches(m.id, selectedModel) ? ' on' : ''}`}
              title={[m.id, m.description, m.is_default ? t('模型預設') : ''].filter(Boolean).join(' — ')}
              onClick={() => pickModel(m.id)}
            >
              {modelLabel(m, kind)}
              {missingIds.has(m.id) ? <span className="field-note"> {t('不在清單')}</span> : latest.oldIds.has(m.id) ? <span className="field-note"> {t('舊版')}</span> : null}
            </button>
          ))}
        </div>
      </div>

      {efforts.length > 0 ? (
        <div className="field">
          <span>
            {t('強度')}
            {}
            {current && current.id !== model && kind !== 'claude' && !agy ? (
              <span className="field-note">{t('依 {model}', { model: current.display_name })}</span>
            ) : null}
            {}
          </span>
          <div className="opt-group" role="radiogroup" aria-label={t('強度')}>
            {efforts.map((e) => {
              const v = agy ? selGroup?.variants.get(e) ?? null : null
              return (
                <button
                  key={e}
                  type="button"
                  role="radio"
                  aria-checked={selectedEffort === e}
                  className={`opt${selectedEffort === e ? ' on' : ''}`}
                  title={agy ? (v?.id ?? e) : (effortTitle(kind, current, e) ?? e)}
                  onClick={() => { if (agy) { if (v) pickModel(v.id) } else onEffort(e) }}
                >
                  {agy ? formatAgyEffortLabel(e) : effortLabel(e)}
                  {agy ? (v && missingIds.has(v.id) && !selGroupAllMissing ? <span className="field-note"> {t('不在清單')}</span> : v && latest.oldIds.has(v.id) ? <span className="field-note"> {t('舊版')}</span> : null) : effortBadges(current, e, suggestedEffort)}
                </button>
              )
            })}
          </div>
        </div>
      ) : null}

      {retiredModel ? (
        <span className="hint field-note warn">
          {t('已儲存的模型「{model}」已不提供，啟動時改用 {replacement}。', { model: retiredModel.from, replacement: retiredModel.to })}
          <button type="button" className="mini-btn" onClick={() => pickModel(retiredModel.to)}>
            {t('改用 {model}', { model: retiredModel.to })}
          </button>
        </span>
      ) : null}
      {effortUnsupported ? (
        <span className="hint field-note warn">
          {t('已儲存的強度「{effort}」{model}沒有這一級，啟動時會被拒。', { effort: effortLabel(effort), model: current ? `${current.display_name} ` : '' })}
          <button type="button" className="mini-btn" onClick={() => onEffort(null)}>
            {t('改用預設')}
          </button>
        </span>
      ) : null}
      {fastUnsupported ? (
        <span className="hint field-note warn">
          {t('已儲存的 Fast 在 {model} 上沒有優先佇列，會被忽略。', { model: current ? current.display_name : t('這個模型') })}
          <button type="button" className="mini-btn" onClick={() => onFast(false)}>
            {t('關掉 Fast')}
          </button>
        </span>
      ) : null}
      {hasFast ? (
        <label className="field row fast-row">
          <input type="checkbox" checked={fast} onChange={(e) => onFast(e.target.checked)} />
          <span>
            Fast
            <span className="field-note">
              {current?.service_tiers.find((t) => isFastTier(t.id))?.description || t('優先佇列（service_tier=priority）')}
            </span>
            {}
            {liveFast(kind) ? <span className="field-note live-note">{t('執行中改會即時套用，不用重啟')}</span> : null}
            {}
            {isApiKind(kind) ? <span className="field-note live-note">{t('下一回合起生效；進行中的回合維持原速度')}</span> : null}
          </span>
        </label>
      ) : null}
      {kind === 'grok' && !fromApi && !loading ? (
        <span className="hint">
          <KindTag kind="grok" /> {t('`grok models` 清單無法取得，顯示內建的 grok-4.7 / grok-4.7-build-fast / grok-4.6 / grok-4.5')}
        </span>
      ) : null}
    </>
  )
}

function effectiveDefaultEffort(m: ModelInfo | undefined): string | null {
  return m?.account_default_effort ?? m?.default_effort ?? null
}

function effortBadges(m: ModelInfo | undefined, e: string, suggestedEffort?: string | null): ReactNode {
  return (
    <>
      {suggestedEffort ? (
        suggestedEffort === e ? (
          <span className="effort-recommended" aria-hidden="true">
            {t('建議')}
          </span>
        ) : null
      ) : m?.default_effort === e ? (
        <span className="effort-recommended" aria-hidden="true">
          {t('廠推薦')}
        </span>
      ) : null}
      {m?.account_default_effort === e ? (
        <span className="effort-recommended effort-account" aria-hidden="true">
          {t('此帳號預設')}
        </span>
      ) : null}
    </>
  )
}

function effortTitle(kind: BotKind, m: ModelInfo | undefined, e: string): string | undefined {
  if (m?.account_default_effort === e) return t('此帳號預設（不帶 --effort 會跑這一級）：{effort}', { effort: effortLabel(e) })
  if (m?.default_effort === e) return t('{source}：{effort}', { source: kind === 'claude' ? t('廠推薦') : t('模型預設'), effort: effortLabel(e) })
  return undefined
}

function liveFast(kind: BotKind): boolean {
  return kind === 'codex'
}

export function ModelQuickPicker({
  botId,
  kind,
  host,
  className,
  title,
  children,
}: {
  botId: string
  kind: BotKind
  host: string
  className?: string
  title?: string
  children: ReactNode
}) {
  const t = useT()
  const bot = useStore((s) => s.bots.find((b) => b.id === botId) ?? null)
  const run = useStore((s) => s.runs[botId] ?? null)
  const fast = bot ? runningFast(bot, run) : false
  const identity = bot?.identity ?? null
  const key = `${kind}@${host || 'local'}@${identity || ''}`
  const cached = useStore((s) => s.models[key])
  const meta = useStore((s) => s.modelsMeta[key])
  const loadModels = useStore((s) => s.loadModels)
  const patchBot = useStore((s) => s.patchBot)
  const notify = useStore((s) => s.notify)
  const patching = useStore((s) => Boolean(s.busy[`patch:${botId}`]))
  const [open, setOpen] = useState(false)
  const [pos, setPos] = useState<{ left: number; top: number } | null>(null)
  const btnRef = useRef<HTMLButtonElement>(null)
  const popRef = useRef<HTMLDivElement>(null)

  useEffect(() => {
    if (open && cached == null) void loadModels(kind, host, identity)
  }, [open, cached, kind, host, identity, loadModels])

  useLayoutEffect(() => {
    if (!open) {
      setPos(null)
      return
    }
    const place = () => {
      const el = btnRef.current
      const pop = popRef.current
      if (!el) return
      const r = el.getBoundingClientRect()
      const margin = 8
      const w = pop?.offsetWidth ?? 260
      const h = pop?.offsetHeight ?? 180
      let left = r.left
      if (left + w + margin > window.innerWidth) left = window.innerWidth - w - margin
      left = Math.max(margin, left)
      let top = r.bottom + 4
      if (top + h + margin > window.innerHeight) top = r.top - 4 - h
      top = Math.max(margin, top)
      setPos({ left, top })
    }
    place()
    window.addEventListener('resize', place)
    window.addEventListener('scroll', place, true)
    return () => {
      window.removeEventListener('resize', place)
      window.removeEventListener('scroll', place, true)
    }
  }, [open, cached])

  useEffect(() => {
    if (!open) return
    const onDoc = (e: MouseEvent) => {
      const t = e.target as Node
      if (btnRef.current?.contains(t) || popRef.current?.contains(t)) return
      setOpen(false)
    }
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setOpen(false)
    }
    document.addEventListener('mousedown', onDoc)
    document.addEventListener('keydown', onKey)
    return () => {
      document.removeEventListener('mousedown', onDoc)
      document.removeEventListener('keydown', onKey)
    }
  }, [open])

  const last = Array.isArray(cached) ? null : lastKnownModels(kind, modelsKey(kind, host, identity))
  const apiModels = apiModelList(kind, Array.isArray(cached) ? cached : last)
  const usingLast = last !== null && apiModels.length > 0
  const model = bot?.model ?? null
  const fromApi = apiModels.length > 0
  const rawModels = fromApi ? apiModels : staticModels(kind)
  const { models, missingIds } = ensureCurrentModel(rawModels, model, kind)
  const loading = open && cached === undefined
  const latest = latestModels(kind, visibleModels(models, model), model)
  const shownModels = sortModels(kind, latest.models)
  const effort = bot?.effort ?? null
  const current = (model && (models.find((m) => m.id === model) ?? models.find((m) => modelMatches(m.id, model)))) || models.find((m) => m.is_default) || models[0]
  const selectedModel = model ?? current?.id ?? null
  const agy = kind === 'agy'
  const agyGroups = agy ? groupAgyModels(shownModels) : []
  const selGroup = agy ? (agyGroupOf(agyGroups, selectedModel) ?? agyGroups.find((g) => g.isDefault) ?? agyGroups[0] ?? null) : null
  const selGroupEffort = agyEffortOf(selGroup, selectedModel)
  const selGroupAllMissing = selGroup ? agyGroupMembers(selGroup).every((v) => missingIds.has(v.id)) : false
  const efforts = agy ? (selGroup?.efforts ?? []) : (current?.efforts ?? [])
  const selectedEffort = agy ? selGroupEffort : (effort ?? effectiveDefaultEffort(current))

  const apply = async (input: PatchBotInput) => {
    const res = await patchBot(botId, input)
    if (!res) return
    if (res.needsRestart) {
      notify('info', t('已儲存，重啟 Bot 後才會套用'))
    }
  }

  const pickModel = (id: string) => {
    if (!bot || patching || id === bot.model || modelMatches(id, bot.model)) {
      setOpen(false)
      return
    }
    setOpen(false)
    void apply(modelSwitchPatch({ effort, fast: bot.fast }, models, id, fromApi))
  }

  const pickEffort = (id: string | null) => {
    if (!bot || patching || id === bot.effort) {
      setOpen(false)
      return
    }
    setOpen(false)
    void apply({ effort: id })
  }

  const toggleFast = () => {
    if (!bot || patching) {
      setOpen(false)
      return
    }
    setOpen(false)
    void apply({ fast: !fast })
  }

  const hasFast = current?.service_tiers.some((t) => isFastTier(t.id)) ?? false
  const warnText = fallbackWarning(t, kind, meta, usingLast, cached === null, fromApi)

  const menuKeys = useMenuKeys(open, popRef, btnRef, () => setOpen(false), cached)
  const hintId = useId()

  const pop = open ? (
      <div
        ref={popRef}
        className="model-quick-pop"
        role="menu"
        aria-label={t('選擇模型')}
        aria-busy={loading || undefined}
        aria-describedby={hintId}
        tabIndex={-1}
        onKeyDown={menuKeys}
        style={pos ? { left: pos.left, top: pos.top } : { left: 0, top: 0, visibility: 'hidden' }}
      >
        {loading ? (
          <span className="hint" aria-hidden="true">
            {t('載入模型清單…')}
          </span>
        ) : warnText ? (
          <span className="hint field-note warn" aria-hidden="true">
            {warnText}
          </span>
        ) : null}
        <div className="opt-group models" role="group" aria-label={t('模型')}>
          {agy ? agyGroups.map((g) => {
            const on = selGroup === g
            const members = agyGroupMembers(g)
            const allMissing = members.every((v) => missingIds.has(v.id))
            const allOld = members.every((v) => latest.oldIds.has(v.id))
            const target = pickAgyVariant(g, selGroupEffort)
            return (
              <button
                key={g.key}
                type="button"
                role="menuitemradio"
                aria-checked={on}
                tabIndex={-1}
                className={`opt${on ? ' on' : ''}`}
                title={[target?.id ?? g.key, g.isDefault ? t('模型預設') : ''].filter(Boolean).join(' — ')}
                disabled={patching}
                onClick={() => { if (target) pickModel(target.id) }}
              >
                {g.label}
                {allMissing ? <span className="field-note"> {t('不在清單')}</span> : allOld ? <span className="field-note"> {t('舊版')}</span> : null}
              </button>
            )
          }) : shownModels.map((m) => (
            <button
              key={m.id}
              type="button"
              role="menuitemradio"
              aria-checked={selectedModel === m.id || modelMatches(m.id, selectedModel)}
              tabIndex={-1}
              className={`opt${selectedModel === m.id || modelMatches(m.id, selectedModel) ? ' on' : ''}`}
              title={[m.id, m.description, m.is_default ? t('模型預設') : ''].filter(Boolean).join(' — ')}
              disabled={patching}
              onClick={() => pickModel(m.id)}
            >
              {modelLabel(m, kind)}
              {missingIds.has(m.id) ? <span className="field-note"> {t('不在清單')}</span> : latest.oldIds.has(m.id) ? <span className="field-note"> {t('舊版')}</span> : null}
            </button>
          ))}
        </div>
        {efforts.length > 0 ? (
          <div className="field">
            <span aria-hidden="true">{t('強度')}</span>
            <div className="opt-group" role="group" aria-label={t('強度')}>
              {efforts.map((e) => {
                const v = agy ? selGroup?.variants.get(e) ?? null : null
                return (
                  <button
                    key={e}
                    type="button"
                    role="menuitemradio"
                    aria-checked={selectedEffort === e}
                    tabIndex={-1}
                    className={`opt${selectedEffort === e ? ' on' : ''}`}
                    disabled={patching}
                    title={agy ? (v?.id ?? e) : effortTitle(kind, current, e)}
                    onClick={() => { if (agy) { if (v) pickModel(v.id) } else pickEffort(e) }}
                  >
                    {agy ? formatAgyEffortLabel(e) : effortLabel(e)}
                    {agy ? (v && missingIds.has(v.id) && !selGroupAllMissing ? <span className="field-note"> {t('不在清單')}</span> : v && latest.oldIds.has(v.id) ? <span className="field-note"> {t('舊版')}</span> : null) : effortBadges(current, e)}
                  </button>
                )
              })}
            </div>
          </div>
        ) : null}
        {hasFast ? (
          <div className="field">
            <span aria-hidden="true">tier</span>
            <div className="opt-group" role="group" aria-label="tier">
              <button
                type="button"
                role="menuitemcheckbox"
                aria-checked={fast}
                tabIndex={-1}
                className={`opt${fast ? ' on' : ''}`}
                disabled={patching}
                title={current?.service_tiers.find((t) => isFastTier(t.id))?.description || t('優先佇列（service_tier=priority）')}
                onClick={toggleFast}
              >
                Fast
              </button>
              <span className="field-note" aria-hidden="true">
                {current?.service_tiers.find((t) => isFastTier(t.id))?.description || '2x speed, increased usage'}
              </span>
            </div>
          </div>
        ) : null}
        <span className="hint" id={hintId} aria-hidden="true">
          {isApiKind(kind) ? t('改模型、強度或 fast 下一回合起生效；進行中的回合維持原設定') : liveFast(kind) ? t('執行中改模型、強度或 fast 都會即時套用') : t('執行中改模型或強度會即時套用')}
        </span>
      </div>
    ) : null

  return (
    <>
      <button
        ref={btnRef}
        type="button"
        className={className}
        title={title ?? t('點一下改模型')}
        aria-haspopup="menu"
        aria-expanded={open}
        aria-label={t('改模型')}
        onClick={() => setOpen((v) => !v)}
      >
        {children}
      </button>
      {pop && createPortal(pop, document.body)}
    </>
  )
}
