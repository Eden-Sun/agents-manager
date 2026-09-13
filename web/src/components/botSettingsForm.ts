import type { BotKind, PatchBotInput } from '../api/types'

/**
 * Bot 設定面板的純函式（docs/reviews/2026-09-12/web.md §1）。只有 `touched` 欄位算數，其餘跟 `base`：
 * 別處改了同一顆 bot 時，儲存才不會用開啟當下的舊值蓋回去。
 */

export type BotFormKey = 'name' | 'model' | 'effort' | 'fast' | 'persona' | 'identity'

export interface BotFormValues {
  name: string
  model: string | null
  effort: string | null
  fast: boolean
  /** 表單裡的原文；比對時空白 → null。 */
  persona: string
  /** `''` = 不指定。 */
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

/** 畫面上要顯示的值：動過的用表單的，沒動過的跟 base 走。 */
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

/** 動過且跟 base 不同的欄位才進 patch。`fast` 只有 codex；`identity` 三種 kind 都收。 */
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
  if (kind === 'codex' && touched.has('fast') && v.fast !== base.fast) patch.fast = v.fast
  const persona = v.persona.trim() || null
  if (touched.has('persona') && persona !== base.persona) patch.persona = persona
  const identity = v.identity || null
  if (touched.has('identity') && identity !== base.identity) patch.identity = identity
  return patch
}
