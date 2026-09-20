import { INSTRUCTION_FILES_DEFAULT, type BotKind, type InstructionFiles, type PatchBotInput } from '../api/types'

/**
 * Bot 設定面板的純函式（docs/reviews/2026-09-12/web.md §1）。只有 `touched` 欄位算數，其餘跟 `base`：
 * 別處改了同一顆 bot 時，儲存才不會用開啟當下的舊值蓋回去。
 */

/** 專案指示檔的四個選項（面板順序）；`hint` 是選中時顯示的說明。值必須跟 daemon／CLI 的 `instructionFiles` 一致（API.md §10）。 */
export const INSTRUCTION_FILES_CHOICES: readonly { value: InstructionFiles; label: string; hint: string }[] = [
  { value: 'claude-md', label: '只讀 CLAUDE.md（預設）', hint: '只讀 CLAUDE.md，不讀寫給 codex 的 AGENTS.md。' },
  {
    value: 'claude-md-or-agents-md',
    label: 'CLAUDE.md，沒有才讀 AGENTS.md',
    hint: '專案有 CLAUDE.md 就只讀它；沒有才改讀 AGENTS.md（claude 2.1.277 的原生預設）。',
  },
  {
    value: 'claude-md-and-agents-md',
    label: '兩份都讀（跟 codex 共用）',
    hint: 'CLAUDE.md 與 AGENTS.md 都讀（CLAUDE.md 已經引用的檔不會讀兩次）。適合讓這顆 claude 和同專案的 codex 共用同一份 AGENTS.md。',
  },
  {
    value: 'managed-only',
    label: '都不讀',
    hint: '專案與使用者自己的指示檔全部不讀，只留組織管理的 CLAUDE.md 與 memory。',
  },
]

export type BotFormKey = 'name' | 'model' | 'effort' | 'fast' | 'persona' | 'identity' | 'instruction_files'

export interface BotFormValues {
  name: string
  model: string | null
  effort: string | null
  fast: boolean
  /** 表單裡的原文；比對時空白 → null。 */
  persona: string
  /** `''` = 不指定。 */
  identity: string
  /** claude 才有；面板永遠是四個值之一（沒設＝預設）。 */
  instruction_files: InstructionFiles
}

export interface BotFormBase {
  name: string
  model: string | null
  effort: string | null
  fast: boolean
  persona: string | null
  identity: string | null
  /** null = 這顆 bot 沒有這個設定（codex／grok、舊 daemon）。 */
  instruction_files: InstructionFiles | null
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
    instruction_files: touched.has('instruction_files') ? form.instruction_files : (base.instruction_files ?? INSTRUCTION_FILES_DEFAULT),
  }
}

/** 動過且跟 base 不同的欄位才進 patch。`fast` 只有 codex；`identity` 三種 kind 都收；`instruction_files` 只有 claude 且 daemon 有給這一格（`base` 不是 null）。 */
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
  if (kind === 'claude' && base.instruction_files !== null && touched.has('instruction_files') && v.instruction_files !== base.instruction_files) {
    patch.instruction_files = v.instruction_files
  }
  return patch
}

const SAVED_KEYS: readonly BotFormKey[] = ['name', 'model', 'effort', 'fast', 'persona', 'identity', 'instruction_files']

/**
 * `saved`（已存成功、store 還沒跟上的欄位）只該撐到 store 追上為止。留著不清的話，之後別處
 * 把同一欄改掉（例如標題列的模型快速選單），面板仍照 `saved` 顯示舊值。
 * store 的值等於存過的值就把那一欄從 `saved` 拿掉；沒有東西可拿就回傳同一個物件（讓 render 期 setState 不會迴圈）。
 */
export function pruneSaved(saved: PatchBotInput, bot: BotFormBase): PatchBotInput {
  let next: PatchBotInput | null = null
  for (const k of SAVED_KEYS) {
    if (!(k in saved)) continue
    const s = saved[k]
    const caughtUp =
      k === 'instruction_files'
        ? bot.instruction_files !== null && (saved.instruction_files ?? INSTRUCTION_FILES_DEFAULT) === bot.instruction_files
        : k === 'fast'
          ? Boolean(saved.fast) === bot.fast
          : (s ?? null) === (bot[k] ?? null)
    if (!caughtUp) continue
    if (!next) next = { ...saved }
    delete next[k]
  }
  return next ?? saved
}
