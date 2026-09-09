import { useEffect, useLayoutEffect, useRef, useState } from 'react'
import type { ReactNode } from 'react'
import { createPortal } from 'react-dom'
import type { BotKind, ModelInfo, PatchBotInput } from '../api/types'
import { CLAUDE_EFFORT_OPTIONS, CODEX_EFFORT_OPTIONS, EFFORT_OPTIONS, FAST_TIER, HIDDEN_MODELS, MODEL_OPTIONS, effortLabel } from '../api/types'
import { useStore } from '../store/store'
import { KindTag } from './KindTag'

/**
 * v4.0 model / effort / fast, driven by `GET /api/models?kind=&host=`.
 * Claude is static on the daemon (opus / sonnet / haiku / fable, all with the same five
 * `--effort` levels — claude has no per-model list the way codex does);
 * codex and grok come from their CLIs. When the call fails the static
 * `MODEL_OPTIONS` list (and kind-specific efforts) is used instead, so the
 * form still works against an older daemon.
 */

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

/**
 * 選單要顯示的清單：拿掉 `HIDDEN_MODELS`（見 types.ts），但**留下這顆 bot 現在選的那個**——
 * 藏掉一個已經設好的值會讓它靜靜變成「自訂」，看起來像設定被改掉了。
 */
function visibleModels(models: ModelInfo[], selected: string | null): ModelInfo[] {
  return models.filter((m) => m.id === selected || !HIDDEN_MODELS.includes(m.id))
}

/**
 * 選單順序照 `MODEL_OPTIONS`（claude 是 haiku → sonnet → opus → fable，由輕到重）。
 *
 * API 回來的順序是 CLI 自己的，會隨版本換位置；一排按鈕的位置天天變，肌肉記憶就沒了。
 * 清單上有、`MODEL_OPTIONS` 沒有的（新模型）維持 API 順序接在後面，不會被藏起來。
 */
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
  /**
   * claude only：這個 bot／角色目前選的身份。只影響「預設」那顆按鈕顯示的提示（那個身份
   * 的 `settings.json` 目前設的強度，SPEC §17.1）——不指定就是預設帳號。
   */
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
  // The effort / fast rows follow the chosen model (or the CLI default when unset).
  const current = (model && models.find((m) => m.id === model)) || defaultModel
  const efforts = current?.efforts ?? []
  // 沒設就顯示「實際會跑的那一個」：模型是 CLI 預設那顆，強度是該模型的廠推薦。
  const selectedModel = model ?? defaultModel?.id ?? null
  const selectedEffort = effort ?? current?.default_effort ?? null
  const hasFast = current?.service_tiers.some((t) => t.id === FAST_TIER) ?? false

  // Drop Fast when the chosen model has no priority tier. Only ever act on a real API list:
  // while the list is loading (or when the fetch failed) `models` is the static fallback, and
  // letting it "correct" the stored value wipes Fast whenever the key changes — e.g. picking
  // another identity, which must not touch anything but `identity`.
  useEffect(() => {
    if (!fromApi) return
    if (!hasFast && fast) onFast(false)
  }, [fromApi, hasFast, fast, onFast])

  // Same for the effort: the levels are per-model, so one carried over from the previously
  // selected model can be rejected outright — codex answers `-c model_reasoning_effort="max"`
  // on gpt-5.5 with `400 unsupported_value`. Falling back to null just omits the flag.
  // This also heals a bot whose stored effort predates a model change, once its panel opens.
  useEffect(() => {
    if (!fromApi) return
    if (effort !== null && efforts.length > 0 && !efforts.includes(effort)) onEffort(null)
  }, [fromApi, efforts, effort, onEffort])

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
          {/* 三個 kind 的 TUI 都能當場換模型，daemon 會直接操作（codex 走 `/model` 選單）。 */}
          {liveModel(kind) ? <span className="field-note live-note primary">執行中改會即時套用，不用重啟</span> : null}
        </span>
        {/* 「使用 CLI 預設」與「自訂…」都拿掉（2026-09-09 使用者決定）：清單上就那幾顆，
            多一顆「預設」等於要人先猜它是誰，多一顆「自訂」則是幾乎沒人走、卻天天佔一格的路。
            沒設 model 的 bot 直接把 CLI 預設那顆標成選取中——它本來就是會跑的那一個。 */}
        <div className="opt-group models" role="radiogroup" aria-label="model">
          {shownModels.map((m) => (
            <button
              key={m.id}
              type="button"
              className={`opt${selectedModel === m.id ? ' on' : ''}`}
              title={[m.id, m.description, m.is_default ? '模型預設' : ''].filter(Boolean).join(' — ')}
              onClick={() => pickModel(m.id)}
            >
              {m.display_name}
            </button>
          ))}
        </div>
      </div>

      {efforts.length > 0 ? (
        <div className="field">
          <span>
            強度
            {/* claude 的五級跟模型無關（`claude --help` 就那一組），標「依 <模型>」會是假資訊。 */}
            {current && current.id !== model && kind !== 'claude' ? (
              <span className="field-note">依 {current.display_name}</span>
            ) : null}
            {/* grok / claude 的 TUI 有 `/effort <level>`，codex 走 `/model` 的第二層選單；
                三個都是 daemon 直接操作。claude 與 codex 會順手把它存成該帳號的預設
                （CLI 行為，見 SPEC §17 與 §4.4a）。 */}
            {liveEffort(kind) ? (
              <span className="field-note live-note" title={kind === 'claude' ? 'claude 會同時把它存成之後新 session 的預設強度' : undefined}>
                執行中改會即時套用，不用重啟
              </span>
            ) : null}
          </span>
          {/* 同模型：不放「預設」那一顆，沒設就把廠推薦的那一級標成選取中。 */}
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

      {hasFast ? (
        <label className="field row fast-row">
          <input type="checkbox" checked={fast} onChange={(e) => onFast(e.target.checked)} />
          <span>
            Fast
            <span className="field-note">
              {current?.service_tiers.find((t) => t.id === FAST_TIER)?.description || '優先佇列（service_tier=priority）'}
            </span>
            {/* codex 的 `/fast` 是執行中就能切的開關（SPEC §4.4a）。 */}
            {liveFast(kind) ? <span className="field-note live-note">執行中改會即時套用，不用重啟</span> : null}
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


/**
 * 「預設」按鈕括號裡那個值是從哪來的：codex / grok 是那個模型自己回報的
 * `default_effort`（API / cache 檔），claude 則是**帳號的 `settings.json`**
 * （`effortLevel` 或 per-model override，SPEC §17.1）——不是模型內建的，講清楚才不會
 * 誤以為換帳號也不會變。
 */
function defaultEffortNote(kind: BotKind): string {
  return kind === 'claude' ? '帳號目前設定' : '模型預設'
}

/**
 * 三個 kind 的 TUI 都能在執行中換模型與強度，daemon 會直接操作（SPEC §4.4a）：
 * claude / grok 是一行 slash 指令，codex 0.153.4 是 `/model` 的兩層選單（daemon 讀畫面選號碼，
 * 再回讀狀態列確認）。套不進去時 `PATCH` 還是會回 `needs_restart`，標題列就出現「需重啟」。
 */
function liveModel(_kind: BotKind): boolean {
  return true
}

function liveEffort(_kind: BotKind): boolean {
  return true
}

/** codex 的 `/fast` 是個開關，執行中也切得掉；其他 kind 根本沒有這個旗標。 */
function liveFast(kind: BotKind): boolean {
  return kind === 'codex'
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

  /** 同 `bots.fast`、同一條 `PATCH`——設定面板那顆勾與這顆 chip 是同一個欄位。 */
  const toggleFast = () => {
    if (!bot || patching) {
      setOpen(false)
      return
    }
    setOpen(false)
    void apply({ fast: !bot.fast })
  }

  // 這個模型有沒有 fast tier（`model/list` 的 `serviceTiers`），跟設定面板同一條判斷。
  const hasFast = current?.service_tiers.some((t) => t.id === FAST_TIER) ?? false

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
          {shownModels.map((m) => (
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
            <span>tier</span>
            <div className="opt-group">
              <button
                type="button"
                role="switch"
                aria-checked={bot?.fast === true}
                className={`opt${bot?.fast ? ' on' : ''}`}
                disabled={patching}
                title={current?.service_tiers.find((t) => t.id === FAST_TIER)?.description || '優先佇列（service_tier=priority）'}
                onClick={toggleFast}
              >
                Fast
              </button>
              <span className="field-note">
                {current?.service_tiers.find((t) => t.id === FAST_TIER)?.description || '2x speed, increased usage'}
              </span>
            </div>
          </div>
        ) : null}
        <span className="hint">
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
