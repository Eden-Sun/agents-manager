import type { BotKind, PatchBotInput } from '../api/types'

export type BotFormKey = 'name' | 'model' | 'effort' | 'fast' | 'persona' | 'identity'

export interface BotFormValues {
  name: string
  model: string | null
  effort: string | null
  fast: boolean
  persona: string
  identity: string
}

export interface BotFormBase {
  name: string
  model: string | null
  effort: string | null
  fast: boolean
  persona: string | null
  identity: string | null
}

export function effectiveForm(base: BotFormBase, form: BotFormValues, touched: ReadonlySet<BotFormKey>): BotFormValues {
  return {
    name: touched.has('name') ? form.name : base.name,
    model: touched.has('model') ? form.model : base.model,
    effort: touched.has('effort') ? form.effort : base.effort,
    fast: touched.has('fast') ? form.fast : base.fast,
    persona: touched.has('persona') ? form.persona : (base.persona ?? ''),
    identity: touched.has('identity') ? form.identity : (base.identity ?? ''),
  }
}

export function computeBotPatch(
  base: BotFormBase,
  form: BotFormValues,
  touched: ReadonlySet<BotFormKey>,
  kind: BotKind,
): PatchBotInput {
  const v = effectiveForm(base, form, touched)
  const patch: PatchBotInput = {}
  if (touched.has('name') && v.name !== base.name) patch.name = v.name
  if (touched.has('model') && v.model !== base.model) patch.model = v.model
  if (touched.has('effort') && v.effort !== base.effort) patch.effort = v.effort
  if ((kind === 'codex' || kind === 'claude-api') && touched.has('fast') && v.fast !== base.fast) patch.fast = v.fast
  const persona = v.persona.trim() || null
  if (touched.has('persona') && persona !== base.persona) patch.persona = persona
  const identity = v.identity || null
  if (touched.has('identity') && identity !== base.identity) patch.identity = identity
  return patch
}

export function settledKeys(sent: PatchBotInput, now: BotFormValues): BotFormKey[] {
  const out: BotFormKey[] = []
  if ('name' in sent && now.name === sent.name) out.push('name')
  if ('model' in sent && now.model === (sent.model ?? null)) out.push('model')
  if ('effort' in sent && now.effort === (sent.effort ?? null)) out.push('effort')
  if ('fast' in sent && now.fast === Boolean(sent.fast)) out.push('fast')
  if ('persona' in sent && (now.persona.trim() || null) === (sent.persona ?? null)) out.push('persona')
  if ('identity' in sent && (now.identity || null) === (sent.identity ?? null)) out.push('identity')
  return out
}

const SAVED_KEYS: readonly BotFormKey[] = ['name', 'model', 'effort', 'fast', 'persona', 'identity']

export function pruneSaved(saved: PatchBotInput, bot: BotFormBase): PatchBotInput {
  let next: PatchBotInput | null = null
  for (const k of SAVED_KEYS) {
    if (!(k in saved)) continue
    const s = saved[k]
    const caughtUp =
      k === 'fast' ? Boolean(saved.fast) === bot.fast : (s ?? null) === (bot[k] ?? null)
    if (!caughtUp) continue
    if (!next) next = { ...saved }
    delete next[k]
  }
  return next ?? saved
}

