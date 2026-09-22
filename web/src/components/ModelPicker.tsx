import { useEffect, useId, useLayoutEffect, useRef, useState } from 'react'
import type { ReactNode } from 'react'
import { createPortal } from 'react-dom'
import type { BotKind, ModelInfo, PatchBotInput } from '../api/types'
import { CLAUDE_EFFORT_OPTIONS, CODEX_EFFORT_OPTIONS, EFFORT_OPTIONS, FAST_TIER, HIDDEN_MODELS, MODEL_OPTIONS, effortLabel } from '../api/types'
import { useMenuKeys } from '../hooks/useMenuKeys'
import { modelSwitchPatch } from '../lib/modelSwitch'
import { useStore } from '../store/store'
import { KindTag } from './KindTag'
import './modelPicker.css'

/** v4.0 model / effort / fast from `GET /api/models?kind=&host=`; falls back to static `MODEL_OPTIONS` on failure. */

function staticModels(kind: BotKind): ModelInfo[] {
  const efforts =
    kind === 'grok' ? [...EFFORT_OPTIONS] : kind === 'codex' ? [...CODEX_EFFORT_OPTIONS] : [...CLAUDE_EFFORT_OPTIONS]
  return MODEL_OPTIONS[kind].map((id, i) => ({
    id,
    display_name: id,
    description: '',
    is_default: i === 0,
    default_effort: null,
    efforts,
    service_tiers: [],
  }))
}

/** 拿掉 `HIDDEN_MODELS` 但留下目前選的，否則看起來像設定被改掉。 */
function visibleModels(models: ModelInfo[], selected: string | null): ModelInfo[] {
  return models.filter((m) => m.id === selected || !HIDDEN_MODELS.includes(m.id))
}

/** 按鈕字拿掉 `gpt-` 前綴省寬度，完整 id 在 `title`（2026-09-09 使用者決定）。 */
function modelLabel(m: ModelInfo): string {
  const name = m.display_name || m.id
  // display_name 是大寫 `GPT-`，id 是小寫。
  return name.replace(/^gpt-/i, '')
}

/** 照 `MODEL_OPTIONS` 排序：API 順序隨 CLI 版本變會毀掉肌肉記憶；新模型接在後面。 */
function sortModels(kind: BotKind, models: ModelInfo[]): ModelInfo[] {
  const want = MODEL_OPTIONS[kind] ?? []
  const rank = (id: string) => {
    const at = want.indexOf(id)
    return at < 0 ? want.length : at
  }
  return models
    .map((m, i) => ({ m, i }))
    .sort((a, b) => rank(a.m.id) - rank(b.m.id) || a.i - b.i)
    .map((x) => x.m)
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
}: {
  kind: BotKind
  host: string
  /** claude only：身份，只影響預設強度提示（SPEC §17.1）。 */
  identity?: string | null
  model: string | null
  onModel: (v: string | null) => void
  effort: string | null
  onEffort: (v: string | null) => void
  fast: boolean
  onFast: (v: boolean) => void
}) {
  const key = `${kind}@${host || 'local'}@${identity || ''}`
  const cached = useStore((s) => s.models[key])
  const loadModels = useStore((s) => s.loadModels)
  useEffect(() => {
    // null = last fetch failed; the store retries once the cooldown has passed (issue #26).
    if (cached == null) void loadModels(kind, host, identity)
  }, [cached, kind, host, identity, loadModels])

  const loading = cached === undefined
  const fromApi = Array.isArray(cached) && cached.length > 0
  const models = fromApi ? cached : staticModels(kind)
  const shownModels = sortModels(kind, visibleModels(models, model))
  const defaultModel = models.find((m) => m.is_default) ?? models[0]
  const current = (model && models.find((m) => m.id === model)) || defaultModel
  const efforts = current?.efforts ?? []
  const selectedModel = model ?? defaultModel?.id ?? null
  const selectedEffort = effort ?? current?.default_effort ?? null
  const hasFast = current?.service_tiers.some((t) => t.id === FAST_TIER) ?? false

  // 不支援的存值只標出來、不自動清（自動清會讓表單無故 dirty，docs/reviews/2026-09-12/web.md §1 ModelPicker）。
  // 只認 API 清單：靜態退路會把好的值誤標不支援。
  const effortUnsupported = fromApi && effort !== null && efforts.length > 0 && !efforts.includes(effort)
  const fastUnsupported = fromApi && fast && !hasFast

  const pickModel = (id: string | null) => {
    onModel(id)
    const next = (id && models.find((m) => m.id === id)) || defaultModel
    if (!(next?.service_tiers.some((t) => t.id === FAST_TIER) ?? false) && fast) onFast(false)
    if (effort !== null && !(next?.efforts ?? []).includes(effort)) onEffort(null)
  }

  return (
    <>
      <div className="field">
        <span>
          模型
          {loading ? <span className="field-note">載入中…</span> : fromApi ? null : <span className="field-note warn">API 不可用，使用內建清單</span>}
          {/* 不標「即時套用」（2026-09-13 使用者：模型與強度不用特別說明）。 */}
        </span>
        {/* 不放「預設」「自訂…」（2026-09-09 使用者決定）；沒設就標 CLI 預設那顆。 */}
        <div className="opt-group models" role="radiogroup" aria-label="model">
          {shownModels.map((m) => (
            <button
              key={m.id}
              type="button"
              className={`opt${selectedModel === m.id ? ' on' : ''}`}
              title={[m.id, m.description, m.is_default ? '模型預設' : ''].filter(Boolean).join(' — ')}
              onClick={() => pickModel(m.id)}
            >
              {modelLabel(m)}
            </button>
          ))}
        </div>
      </div>

      {efforts.length > 0 ? (
        <div className="field">
          <span>
            強度
            {/* claude 的強度與模型無關，標「依 <模型>」是假資訊。 */}
            {current && current.id !== model && kind !== 'claude' ? (
              <span className="field-note">依 {current.display_name}</span>
            ) : null}
            {/* claude／codex 切強度會存成帳號預設（SPEC §17、§4.4a）。 */}
          </span>
          <div className="opt-group" role="radiogroup" aria-label="reasoning effort">
            {efforts.map((e) => (
              <button
                key={e}
                type="button"
                className={`opt${selectedEffort === e ? ' on' : ''}`}
                title={current?.default_effort === e ? `${defaultEffortNote(kind)}：${effortLabel(e)}` : e}
                onClick={() => onEffort(e)}
              >
                {effortLabel(e)}
                {current?.default_effort === e ? (
                  <span className="effort-recommended" aria-hidden="true">
                    廠推薦
                  </span>
                ) : null}
              </button>
            ))}
          </div>
        </div>
      ) : null}

      {effortUnsupported ? (
        <span className="hint field-note warn">
          存著的強度「{effortLabel(effort)}」{current ? `${current.display_name} ` : ''}沒有這一級，啟動時會被拒。
          <button type="button" className="mini-btn" onClick={() => onEffort(null)}>
            改用預設
          </button>
        </span>
      ) : null}
      {fastUnsupported ? (
        <span className="hint field-note warn">
          存著的 Fast 在{current ? ` ${current.display_name}` : '這個模型'}上沒有優先佇列，會被忽略。
          <button type="button" className="mini-btn" onClick={() => onFast(false)}>
            關掉 Fast
          </button>
        </span>
      ) : null}
      {hasFast ? (
        <label className="field row fast-row">
          <input type="checkbox" checked={fast} onChange={(e) => onFast(e.target.checked)} />
          <span>
            Fast
            <span className="field-note">
              {current?.service_tiers.find((t) => t.id === FAST_TIER)?.description || '優先佇列（service_tier=priority）'}
            </span>
            {/* codex `/fast` 執行中可切（SPEC §4.4a）。 */}
            {liveFast(kind) ? <span className="field-note live-note">執行中改會即時套用，不用重啟</span> : null}
          </span>
        </label>
      ) : null}
      {kind === 'grok' && !fromApi && !loading ? (
        <span className="hint">
          <KindTag kind="grok" /> `grok models` 清單無法取得，顯示內建的 grok-4.7 / grok-4.7-build-fast / grok-4.6 / grok-4.5
        </span>
      ) : null}
    </>
  )
}

/** claude 的預設強度來自帳號 `settings.json`（SPEC §17.1），不是模型內建。 */
function defaultEffortNote(kind: BotKind): string {
  return kind === 'claude' ? '帳號目前設定' : '模型預設'
}

function liveFast(kind: BotKind): boolean {
  return kind === 'codex'
}

/** Compact picker for status line / header; PATCHes immediately. */
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
  const bot = useStore((s) => s.bots.find((b) => b.id === botId) ?? null)
  const identity = bot?.identity ?? null
  const key = `${kind}@${host || 'local'}@${identity || ''}`
  const cached = useStore((s) => s.models[key])
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

  const fromApi = Array.isArray(cached) && cached.length > 0
  const models = fromApi ? cached : staticModels(kind)
  const loading = open && cached === undefined
  const model = bot?.model ?? null
  const shownModels = sortModels(kind, visibleModels(models, model))
  const effort = bot?.effort ?? null
  const current = (model && models.find((m) => m.id === model)) || models.find((m) => m.is_default) || models[0]
  const efforts = current?.efforts ?? []

  const apply = async (input: PatchBotInput) => {
    const needsRestart = await patchBot(botId, input)
    if (needsRestart === null) return
    if (needsRestart) {
      notify('info', '已儲存，重啟 Bot 後才會套用')
    }
  }

  const pickModel = (id: string) => {
    if (!bot || patching || id === bot.model) {
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
    void apply({ fast: !bot.fast })
  }

  const hasFast = current?.service_tiers.some((t) => t.id === FAST_TIER) ?? false

  const menuKeys = useMenuKeys(open, popRef, btnRef, () => setOpen(false), cached)
  const hintId = useId()

  // role=menu 非 listbox：點一下即 PATCH 並關閉；方向鍵只移焦點，否則每按一下送一次 PATCH。
  const pop = open ? (
      <div
        ref={popRef}
        className="model-quick-pop"
        role="menu"
        aria-label="選擇模型"
        aria-busy={loading || undefined}
        aria-describedby={hintId}
        tabIndex={-1}
        onKeyDown={menuKeys}
        style={pos ? { left: pos.left, top: pos.top } : { left: 0, top: 0, visibility: 'hidden' }}
      >
        {loading ? (
          <span className="hint" aria-hidden="true">
            載入模型清單…
          </span>
        ) : null}
        <div className="opt-group models" role="group" aria-label="模型">
          {shownModels.map((m) => (
            <button
              key={m.id}
              type="button"
              role="menuitemradio"
              aria-checked={model === m.id}
              tabIndex={-1}
              className={`opt${model === m.id ? ' on' : ''}`}
              title={[m.id, m.description, m.is_default ? '模型預設' : ''].filter(Boolean).join(' — ')}
              disabled={patching}
              onClick={() => pickModel(m.id)}
            >
              {modelLabel(m)}
            </button>
          ))}
        </div>
        {efforts.length > 0 ? (
          <div className="field">
            <span aria-hidden="true">強度</span>
            <div className="opt-group" role="group" aria-label="強度">
              {efforts.map((e) => (
                <button
                  key={e}
                  type="button"
                  role="menuitemradio"
                  aria-checked={effort === e}
                  tabIndex={-1}
                  className={`opt${effort === e ? ' on' : ''}`}
                  disabled={patching}
                  title={current?.default_effort === e ? `${defaultEffortNote(kind)}：${effortLabel(e)}` : undefined}
                  onClick={() => pickEffort(e)}
                >
                  {effortLabel(e)}
                  {current?.default_effort === e ? (
                    <span className="effort-recommended" aria-hidden="true">
                      廠推薦
                    </span>
                  ) : null}
                </button>
              ))}
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
                aria-checked={bot?.fast === true}
                tabIndex={-1}
                className={`opt${bot?.fast ? ' on' : ''}`}
                disabled={patching}
                title={current?.service_tiers.find((t) => t.id === FAST_TIER)?.description || '優先佇列（service_tier=priority）'}
                onClick={toggleFast}
              >
                Fast
              </button>
              <span className="field-note" aria-hidden="true">
                {current?.service_tiers.find((t) => t.id === FAST_TIER)?.description || '2x speed, increased usage'}
              </span>
            </div>
          </div>
        ) : null}
        <span className="hint" id={hintId} aria-hidden="true">
          {liveFast(kind) ? '執行中改模型、強度或 fast 都會即時套用' : '執行中改模型或強度會即時套用'}
        </span>
      </div>
    ) : null

  return (
    <>
      <button
        ref={btnRef}
        type="button"
        className={className}
        title={title ?? '點一下改模型'}
        aria-haspopup="menu"
        aria-expanded={open}
        aria-label="改模型"
        onClick={() => setOpen((v) => !v)}
      >
        {children}
      </button>
      {pop && createPortal(pop, document.body)}
    </>
  )
}
