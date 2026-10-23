import { useT } from '../lib/i18n'
import { useState } from 'react'
import { EMPTY_INPUT, inputEquals, panelInput, panelShows, parseAsyncScreen } from '../lib/codexAsyncQuestion'
import { runPin, type RunPin } from '../lib/runPin'
import type { PendingQuestion } from '../lib/pendingQuestion'
import { parseChoiceMenu } from '../lib/tuiChoices'
import { useStore } from '../store/store'
import { TypeAnswerField } from './TypeAnswerField'
import './blockedChoices.css'

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms))

/** 送出一步之後等畫面反應：每 `STEP` 看一次，最多看 `TRIES` 次（同 `BlockedChoices`）。 */
const STEP = 260
const TRIES = 5

/**
 * codex 非同步提問（`request_user_input_async`，issue #1879）：畫面只剩 `? 1 question · shift+← to answer`，題目與選項
 * 由上面的原題卡（daemon 讀 rollout）補；這裡只做「展開」與「自由輸入題」的作答，編號選單展開後交給 `BlockedChoices`。
 *
 * 每一步都重讀畫面、對不上就不送：展開要畫面還是收合的、貼字要畫面還是展開的自由輸入、Enter 要字真的出現在畫面上。
 * 使用者的 bot 只做使用者在這裡按下的那一下，不替任何人作答。
 */
export function CodexAsyncQuestion({

  botId,
  text,
  questions = [],
  onAnswered,
}: {
  botId: string
  /** 每秒輪詢的終端快照。 */
  text: string | null | undefined
  /** daemon 從 rollout 讀、且對得上畫面的題目（`pending-question`）；展開後恰好一題才知道「這題」是哪一題。 */
  questions?: PendingQuestion[]
  onAnswered?: () => void
}) {
  const kind = useStore((s) => s.bots.find((b) => b.id === botId)?.kind ?? null)
  const readTerminal = useStore((s) => s.readTerminal)
  const sendKeys = useStore((s) => s.sendKeys)
  const sendText = useStore((s) => s.sendText)
  const [busy, setBusy] = useState(false)
  const t = useT()
  const [note, setNote] = useState<string | null>(null)
  const [answer, setAnswer] = useState('')

  const screen = kind === 'codex' ? parseAsyncScreen(text) : null
  // 展開後有編號選單的交給 `BlockedChoices`；沒有選單（自由輸入題）才在這裡打字。
  const freeText = screen?.expanded === true && parseChoiceMenu(text) === null
  // 面板上的題目原文要跟 rollout 的題目對得上才知道這是哪一題；對不上（或有好幾題）就不給打字框，請使用者在終端作答。
  const target = questions.length === 1 ? questions[0].question : null
  if (!screen || (screen.expanded && !freeText)) return null

  const gone = t('這次作答的 Run 已經換了（bot 重啟過）或讀不到畫面，什麼都沒送出。')

  /** 每次作答一份：固定在開始讀畫面時的 Run（`lib/runPin.ts`），送鍵帶這個 id，Run 不符就放棄、不走 fresh-run 重試。 */
  const newPin = () => runPin(() => readTerminal(botId, 'visible', 200), () => useStore.getState().runs[botId]?.id)

  const run = async (fn: () => Promise<string | null>) => {
    if (busy) return
    setBusy(true)
    setNote(null)
    try {
      setNote(await fn())
    } catch {
      // store.sendKeys／sendText 已經發通知；這裡只在旁邊提示，不再發第二則。
      setNote(t('沒有送出去（Run 可能已經換了），請查看終端畫面後再試。'))
    } finally {
      setBusy(false)
      onAnswered?.()
    }
  }

  const waitFor = async (pin: RunPin, done: (now: string) => boolean): Promise<boolean> => {
    for (let i = 0; i < TRIES; i++) {
      await sleep(STEP)
      const now = await pin.read()
      if (now !== null && done(now)) return true
      if (pin.changed()) return false
    }
    return false
  }

  /** 收合 → 展開：只送一下 shift+←（無副作用，不會作答）。 */
  const expand = () =>
    run(async () => {
      const pin = newPin()
      const now = await pin.read()
      if (now === null) return gone
      const before = parseAsyncScreen(now)
      if (!before || before.expanded) return t('畫面已經不是收合的提問（可能剛被作答或展開了），沒有送出 shift+←。')
      await sendKeys(botId, ['shift+left'], pin.id())
      const ok = await waitFor(pin, (t) => parseAsyncScreen(t)?.expanded === true)
      return ok ? null : pin.changed() ? gone : t('按了 shift+←，但畫面沒有展開；請查看終端畫面。')
    })

  /**
   * 自由輸入題：貼字前輸入框必須是空的（佔位字）、送 Enter 前輸入框的**完整內容**必須等於答案（不是後綴：原本有 Alice 再貼
   * Bob 會是 AliceBob），而且兩次都要提問面板展開、題目完整等於這一題。送鍵都帶固定的 Run。
   */
  const submitText = () =>
    run(async () => {
      const typed = answer.replace(/\s*\n\s*/g, ' ').trim()
      if (!typed) return t('請先打字。')
      if (!target) return t('這題的原文讀不到，什麼都沒送出。')
      const pin = newPin()
      const now = await pin.read()
      if (now === null) return gone
      // 底下還有編號列＝其實是選項選單（只是認不出來）：貼字會落在游標那一列，什麼都不送。
      const numbered = now.split('\n').slice(-14).some((l) => /^\s*[›>]?\s*\d+\.\s/.test(l))
      if (!panelShows(now, target) || parseChoiceMenu(now) !== null || numbered) return t('畫面已經不是這一題的自由輸入（可能切回主輸入框或換題了），什麼都沒送出。')
      if (panelInput(now, target) !== EMPTY_INPUT) return t('這題的輸入框裡原本就有字，不在上面疊字（會變成「舊字＋新字」），什麼都沒送出；請先在終端清掉。')
      if (!(await sendText(botId, typed, false, false, pin.id()))) return t('文字沒有送出去。')
      const shown = await waitFor(pin, (t) => panelShows(t, target) && inputEquals(panelInput(t, target), typed))
      if (!shown) return pin.changed() ? gone : t('輸入框的內容不等於要送的答案，沒有按 Enter；請查看終端畫面。')
      const last = await pin.read()
      if (last === null || !panelShows(last, target) || !inputEquals(panelInput(last, target), typed) || parseChoiceMenu(last) !== null) {
        return pin.changed() ? gone : t('送 Enter 前畫面又變了，沒有按 Enter；請查看終端畫面。')
      }
      await sendKeys(botId, ['enter'], pin.id())
      setAnswer('')
      return null
    })

  return (
    <div className="blocked-modal-update" role="group" aria-label={t("codex 的非同步提問")}>
      {screen.expanded && !target ? (
        <span className="hint">{t("這題的原文讀不到（或對不上），無法確認是哪一題，請直接在終端作答。")}</span>
      ) : screen.expanded ? (
        <>
          <TypeAnswerField value={answer} onChange={setAnswer} disabled={busy} />
          <button type="button" className="mini-btn" disabled={busy || !answer.trim()} title={t("貼進終端的輸入框，確認字出現後送 Enter")} onClick={() => void submitText()}>
            {t("送出答案")}
          </button>
          <span className="hint">{t("這題沒有選項，答案貼進終端的輸入框後按 Enter（換行會變成空白）。")}</span>
        </>
      ) : (
        <>
          <button type="button" className="mini-btn" disabled={busy} title={t("送出 shift+←：展開提問，看得到選項後才能作答")} onClick={() => void expand()}>
            {t("展開作答（shift+←）")}
          </button>
          <span className="hint">
            {t("codex 有 {n} 題非同步提問排在輸入框上面，要先展開才能作答；展開不會替你選任何答案。", { n: screen.count })}
          </span>
        </>
      )}
      {note ? (
        <span className="hint bc-note" role="status">
          {note}
        </span>
      ) : null}
    </div>
  )
}
