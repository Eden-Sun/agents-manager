import { useEffect, useState } from 'react'
import type { BotKind, ModelInfo } from '../api/types'
import { EFFORT_OPTIONS, FAST_TIER, MODEL_OPTIONS } from '../api/types'
import { useStore } from '../store/store'
import { KindTag } from './KindTag'

/**
 * v4.0 model / effort / fast for codex and grok, driven by `GET /api/models?kind=&host=`.
 * When the call fails the static `MODEL_OPTIONS` list (and, for grok, `EFFORT_OPTIONS`)
 * is used instead, so the form still works against an older daemon.
 */

function staticModels(kind: BotKind): ModelInfo[] {
  return MODEL_OPTIONS[kind].map((id, i) => ({
    id,
    display_name: id,
    description: '',
    is_default: i === 0,
    default_effort: null,
    efforts: kind === 'grok' ? [...EFFORT_OPTIONS] : [],
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

  return (
    <>
      <div className="field">
        <span>
          模型
          {loading ? <span className="field-note">載入中…</span> : fromApi ? null : <span className="field-note warn">API 不可用，使用內建清單</span>}
        </span>
        <div className="opt-group models" role="radiogroup" aria-label="model">
          <button
            type="button"
            className={`opt${model === null && !customMode ? ' on' : ''}`}
            title={defaultModel ? `不帶 -m，由 CLI 決定（目前：${defaultModel.display_name}）` : '不帶 -m'}
            onClick={() => {
              setCustom(false)
              onModel(null)
            }}
          >
            （預設）
          </button>
          {models.map((m) => (
            <button
              key={m.id}
              type="button"
              className={`opt${model === m.id && !custom ? ' on' : ''}`}
              title={[m.id, m.description].filter(Boolean).join(' — ')}
              onClick={() => {
                setCustom(false)
                onModel(m.id)
              }}
            >
              {m.display_name}
              {m.is_default ? <span className="opt-note">預設</span> : null}
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
          </span>
          <div className="opt-group" role="radiogroup" aria-label="reasoning effort">
            <button
              type="button"
              className={`opt${effort === null ? ' on' : ''}`}
              title={current?.default_effort ? `不帶 --reasoning-effort（模型預設 ${current.default_effort}）` : '不帶 --reasoning-effort'}
              onClick={() => onEffort(null)}
            >
              預設{current?.default_effort ? `（${current.default_effort}）` : ''}
            </button>
            {efforts.map((e) => (
              <button key={e} type="button" className={`opt${effort === e ? ' on' : ''}`} onClick={() => onEffort(e)}>
                {e}
                {current?.default_effort === e ? <span className="opt-note">預設</span> : null}
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
