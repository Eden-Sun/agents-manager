import { useEffect, useLayoutEffect, useRef, useState } from 'react'
import type { ReactNode } from 'react'
import { createPortal } from 'react-dom'
import type { BotKind, ModelInfo, PatchBotInput } from '../api/types'
import { CODEX_EFFORT_OPTIONS, EFFORT_OPTIONS, FAST_TIER, MODEL_OPTIONS, effortLabel } from '../api/types'
import { useStore } from '../store/store'
import { KindTag } from './KindTag'

/**
 * v4.0 model / effort / fast, driven by `GET /api/models?kind=&host=`.
 * Claude is static on the daemon (opus / sonnet / haiku / fable, no efforts);
 * codex and grok come from their CLIs. When the call fails the static
 * `MODEL_OPTIONS` list (and kind-specific efforts) is used instead, so the
 * form still works against an older daemon.
 */

function staticModels(kind: BotKind): ModelInfo[] {
  const efforts = kind === 'grok' ? [...EFFORT_OPTIONS] : kind === 'codex' ? [...CODEX_EFFORT_OPTIONS] : []
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

export function ApiModelFields({
  kind,
  host,
  model,
  onModel,
  effort,
  onEffort,
  fast,
  onFast,
}: {
  kind: BotKind
  host: string
  model: string | null
  onModel: (v: string | null) => void
  effort: string | null
  onEffort: (v: string | null) => void
  fast: boolean
  onFast: (v: boolean) => void
}) {
  const key = `${kind}@${host || 'local'}`
  const cached = useStore((s) => s.models[key])
  const loadModels = useStore((s) => s.loadModels)
  const [custom, setCustom] = useState(false)

  useEffect(() => {
    if (cached === undefined) void loadModels(kind, host)
  }, [cached, kind, host, loadModels])

  const loading = cached === undefined
  const fromApi = Array.isArray(cached) && cached.length > 0
  const models = fromApi ? cached : staticModels(kind)
  const defaultModel = models.find((m) => m.is_default) ?? models[0]
  const known = model === null || models.some((m) => m.id === model)
  const customMode = custom || !known
  // The effort / fast rows follow the chosen model (or the CLI default when unset).
  const current = (model && models.find((m) => m.id === model)) || defaultModel
  const efforts = current?.efforts ?? []
  const hasFast = current?.service_tiers.some((t) => t.id === FAST_TIER) ?? false

  // Drop Fast when the chosen model (or the static fallback) has no priority tier.
  useEffect(() => {
    if (!hasFast && fast) onFast(false)
  }, [hasFast, fast, onFast])

  // Same for the effort: the levels are per-model, so one carried over from the previously
  // selected model can be rejected outright — codex answers `-c model_reasoning_effort="max"`
  // on gpt-5.5 with `400 unsupported_value`. Falling back to null just omits the flag.
  // This also heals a bot whose stored effort predates a model change, once its panel opens.
  useEffect(() => {
    if (effort !== null && efforts.length > 0 && !efforts.includes(effort)) onEffort(null)
  }, [efforts, effort, onEffort])

  const pickModel = (id: string | null) => {
    setCustom(false)
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
          {/* claude / grok 的 TUI 有 /model，daemon 會直接送進去；換模型不用重啟。 */}
          {kind === 'claude' || kind === 'grok' ? <span className="field-note">執行中改會即時套用，不用重啟</span> : null}
        </span>
        <div className="opt-group models" role="radiogroup" aria-label="model">
          <button
            type="button"
            className={`opt${model === null && !customMode ? ' on' : ''}`}
            title={
              kind === 'claude'
                ? '不帶 --model，由這個專案的 Claude 設定決定'
                : defaultModel
                  ? `不帶 -m，由 CLI 決定（目前：${defaultModel.display_name}）`
                  : '不帶 -m'
            }
            onClick={() => pickModel(null)}
          >
            {kind === 'claude' ? '不指定模型（專案預設）' : '使用 CLI 預設'}
          </button>
          {models.map((m) => (
            <button
              key={m.id}
              type="button"
              className={`opt${model === m.id && !custom ? ' on' : ''}`}
              title={[m.id, m.description, m.is_default ? '模型預設' : ''].filter(Boolean).join(' — ')}
              onClick={() => pickModel(m.id)}
            >
              {m.display_name}
            </button>
          ))}
          <button type="button" className={`opt${customMode ? ' on' : ''}`} onClick={() => setCustom(true)} title="輸入任意模型名稱">
            自訂…
          </button>
        </div>
        {customMode ? (
          <input
            type="text"
            value={model ?? ''}
            placeholder={defaultModel?.id ?? ''}
            spellCheck={false}
            aria-label="自訂模型名稱"
            onChange={(e) => onModel(e.target.value ? e.target.value : null)}
          />
        ) : null}
      </div>

      {efforts.length > 0 ? (
        <div className="field">
          <span>
            強度
            {current && current.id !== model ? <span className="field-note">依 {current.display_name}</span> : null}
            {/* grok 的 TUI 有 /effort，daemon 會直接送進去；codex 只能重啟。 */}
            {kind === 'grok' ? <span className="field-note">執行中改會即時套用，不用重啟</span> : null}
          </span>
          <div className="opt-group" role="radiogroup" aria-label="reasoning effort">
            <button
              type="button"
              className={`opt${effort === null ? ' on' : ''}`}
              title={
                current?.default_effort
                  ? `不帶 --reasoning-effort（模型預設 ${effortLabel(current.default_effort)}）`
                  : '不帶 --reasoning-effort'
              }
              onClick={() => onEffort(null)}
            >
              預設{current?.default_effort ? `（${effortLabel(current.default_effort)}）` : ''}
            </button>
            {efforts.map((e) => (
              <button
                key={e}
                type="button"
                className={`opt${effort === e ? ' on' : ''}`}
                title={current?.default_effort === e ? `模型預設強度：${effortLabel(e)}` : e}
                onClick={() => onEffort(e)}
              >
                {effortLabel(e)}
              </button>
            ))}
          </div>
        </div>
      ) : null}

      {hasFast ? (
        <label className="field row fast-row">
          <input type="checkbox" checked={fast} onChange={(e) => onFast(e.target.checked)} />
          <span>
            Fast
            <span className="field-note">
              {current?.service_tiers.find((t) => t.id === FAST_TIER)?.description || '優先佇列（service_tier=priority）'}
            </span>
          </span>
        </label>
      ) : null}
      {kind === 'grok' && !fromApi && !loading ? (
        <span className="hint">
          <KindTag kind="grok" /> `grok models` 清單無法取得，顯示內建的 grok-4.6 / grok-4.5
        </span>
      ) : null}
    </>
  )
}

/** Kinds whose TUI can take `/model` (or grok `/effort`) without a restart. */
function liveModel(kind: BotKind): boolean {
  return kind === 'claude' || kind === 'grok'
}

function liveEffort(kind: BotKind): boolean {
  return kind === 'grok'
}

/**
 * Compact model (and grok effort) picker, used from the status line and the header
 * model-tag. Choosing an option PATCHes immediately; live kinds apply via slash
 * command, the rest report `needs_restart`.
 */
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
  const key = `${kind}@${host || 'local'}`
  const cached = useStore((s) => s.models[key])
  const loadModels = useStore((s) => s.loadModels)
  const bot = useStore((s) => s.bots.find((b) => b.id === botId) ?? null)
  const patchBot = useStore((s) => s.patchBot)
  const notify = useStore((s) => s.notify)
  const patching = useStore((s) => Boolean(s.busy[`patch:${botId}`]))
  const [open, setOpen] = useState(false)
  const [pos, setPos] = useState<{ left: number; top: number } | null>(null)
  const btnRef = useRef<HTMLButtonElement>(null)
  const popRef = useRef<HTMLDivElement>(null)

  useEffect(() => {
    if (open && cached === undefined) void loadModels(kind, host)
  }, [open, cached, kind, host, loadModels])

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
    const next = models.find((m) => m.id === id)
    const input: PatchBotInput = { model: id }
    if (effort !== null && next && next.efforts.length > 0 && !next.efforts.includes(effort)) {
      input.effort = next.default_effort
    }
    setOpen(false)
    void apply(input)
  }

  const pickEffort = (id: string | null) => {
    if (!bot || patching || id === bot.effort) {
      setOpen(false)
      return
    }
    setOpen(false)
    void apply({ effort: id })
  }

  const pop = open ? (
      <div
        ref={popRef}
        className="model-quick-pop"
        role="listbox"
        aria-label="選擇模型"
        style={pos ? { left: pos.left, top: pos.top } : { left: 0, top: 0, visibility: 'hidden' }}
      >
        {loading ? <span className="hint">載入模型清單…</span> : null}
        <div className="opt-group models" role="radiogroup" aria-label="model">
          {models.map((m) => (
            <button
              key={m.id}
              type="button"
              role="option"
              aria-selected={model === m.id}
              className={`opt${model === m.id ? ' on' : ''}`}
              title={[m.id, m.description, m.is_default ? '模型預設' : ''].filter(Boolean).join(' — ')}
              disabled={patching}
              onClick={() => pickModel(m.id)}
            >
              {m.display_name}
            </button>
          ))}
        </div>
        {efforts.length > 0 ? (
          <div className="field">
            <span>強度</span>
            <div className="opt-group" role="radiogroup" aria-label="reasoning effort">
              {efforts.map((e) => (
                <button
                  key={e}
                  type="button"
                  className={`opt${effort === e ? ' on' : ''}`}
                  disabled={patching}
                  onClick={() => pickEffort(e)}
                >
                  {effortLabel(e)}
                </button>
              ))}
            </div>
          </div>
        ) : null}
        {liveModel(kind) ? (
          <span className="hint">{liveEffort(kind) ? '執行中改模型或強度會即時套用' : '執行中改模型會即時套用'}</span>
        ) : (
          <span className="hint">執行中改了要重啟才生效</span>
        )}
      </div>
    ) : null

  return (
    <>
      <button
        ref={btnRef}
        type="button"
        className={className}
        title={title ?? '點一下改模型'}
        aria-haspopup="listbox"
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
