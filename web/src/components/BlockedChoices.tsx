import { useLayoutEffect, useRef, useState } from 'react'
import { useT } from '../lib/i18n'
import { usePaneKeys } from '../hooks/usePaneKeys'
import {
  customAnswerChecked,
  customAnswerShown,
  keysToMove,
  keysToSelect,
  parseChoiceMenu,
  sameChoices,
  typedAnswerHere,
  type TuiChoiceMenu,
  type WalkTarget,
} from '../lib/tuiChoices'
import { panelShows } from '../lib/codexAsyncQuestion'
import { isSubmitMenu } from '../lib/choiceDraft'
import { runPin, type RunPin } from '../lib/runPin'
import { jevAskNote, jevMatchesTitle, type PendingAt } from '../lib/pendingQuestion'
import { useStore } from '../store/store'
import { JevLabel } from './JevLabel'
import { useMenuTranslation } from './useMenuTranslation'
import { TypeAnswerField } from './TypeAnswerField'
import './blockedChoices.css'

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms))

const squashText = (s: string) => s.replace(/\s+/g, '')

function scroller(el: HTMLElement): HTMLElement | null {
  for (let p = el.parentElement; p; p = p.parentElement) {
    const o = getComputedStyle(p).overflowY
    if ((o === 'auto' || o === 'scroll') && p.scrollHeight > p.clientHeight + 1) return p
  }
  return null
}

const STEP = 260
const TRIES = 3

const WALK_TRIES = 3

type Busy = { kind: 'choice' | 'submit' | 'tab'; key: string } | null

const EDIT_PROMPT_CHOICE = /^Edit prompt and retry\b/i

export function BlockedChoices({
  botId,
  menu,
  onAnswered,
  pendingAt = null,
}: {
  botId: string
  menu: TuiChoiceMenu
  onAnswered?: () => void
  pendingAt?: PendingAt | null
}) {
  const t = useT()
  const jev = pendingAt?.jev ?? null
  const jevRowShown = jev !== null && menu.choices.some((c) => jevMatchesTitle(jev.label, c.title))
  const readTerminal = useStore((s) => s.readTerminal)
  const sendKeys = useStore((s) => s.sendKeys)
  const sendText = useStore((s) => s.sendText)
  const loadComposerDraft = useStore((s) => s.loadComposerDraft)
  const [busy, setBusy] = useState<Busy>(null)
  const [note, setNote] = useState<string | null>(null)
  const [typeText, setTypeText] = useState('')
  const [typeFor, setTypeFor] = useState<number | null>(null)
  const [tabAt, setTabAt] = useState<number | null>(null)
  const pageTabs = menu.tabs.flatMap((t, i) => (t.submit ? [] : [i]))
  const screenTab = !isSubmitMenu(menu) && pendingAt && pendingAt.total === pageTabs.length ? (pageTabs[pendingAt.at] ?? null) : null
  const guessTab = screenTab ?? menu.tabAt
  const guessOnSubmit = guessTab !== null && Boolean(menu.tabs[guessTab]?.submit) && !isSubmitMenu(menu)
  const atTab = tabAt ?? (guessOnSubmit ? null : guessTab)
  const tr = useMenuTranslation([
    {
      header: atTab === null ? '' : (menu.tabs[atTab]?.label ?? ''),
      question: menu.question ?? pendingAt?.question ?? null,
      multi: menu.multi,
      choices: menu.choices.map((c) => ({ title: c.title, detail: c.detail })),
    },
  ])
  const disp = tr.page(0)

  const [open, setOpen] = useState<number[]>([])
  const [openFor, setOpenFor] = useState(menu.question)
  if (openFor !== menu.question) {
    setOpenFor(menu.question)
    setOpen([])
  }

  const listRef = useRef<HTMLOListElement>(null)
  const anchor = useRef<{ el: HTMLElement; top: number } | null>(null)
  const lastCursor = useRef(menu.cursor)

  useLayoutEffect(() => {
    const a = anchor.current
    anchor.current = null
    if (a) {
      const box = scroller(a.el)
      const now = a.el.getBoundingClientRect().top
      if (box) box.scrollTop += now - a.top
      return
    }
    listRef.current?.querySelectorAll<HTMLElement>('.bc-row').forEach((li) => {
      if (li.querySelector('.bc-open')) return
      const t = li.querySelector('.bc-title')
      if (t && t.scrollWidth > t.clientWidth + 1) li.dataset.wide = '1'
      else delete li.dataset.wide
    })
  })

  useLayoutEffect(() => {
    if (lastCursor.current === menu.cursor) return
    lastCursor.current = menu.cursor
    listRef.current
      ?.querySelectorAll<HTMLElement>('.bc-item')
      [menu.cursor]?.scrollIntoView({ block: 'nearest' })
  }, [menu.cursor])

  const toggleDetail = (i: number, el: HTMLElement) => {
    const box = scroller(el)
    if (box) anchor.current = { el, top: el.getBoundingClientRect().top }
    setOpen((v) => (v.includes(i) ? v.filter((n) => n !== i) : [...v, i]))
  }

  const read = async (): Promise<TuiChoiceMenu | null> => {
    try {
      return parseChoiceMenu((await readTerminal(botId, 'visible', 200)).text)
    } catch {
      return null
    }
  }

  const stillHere = (now: TuiChoiceMenu | null): now is TuiChoiceMenu => Boolean(now && sameChoices(now, menu))

  const walkTo = async (target: WalkTarget): Promise<TuiChoiceMenu | null> => {
    for (let i = 0; i < WALK_TRIES; i++) {
      const now = await read()
      if (!stillHere(now)) return null
      const at: WalkTarget | null = now.submit?.current ? 'submit' : now.cursor >= 0 ? now.cursor : null
      if (at === target) return now
      const keys = at === null ? ['up'] : (keysToMove(now, target) ?? [])
      if (!keys.length) return null
      await sendKeys(botId, keys)
      await sleep(STEP)
    }
    return null
  }

  const pressUntil = async (key: string, done: (now: TuiChoiceMenu) => boolean): Promise<boolean> => {
    await sendKeys(botId, [key])
    for (let i = 0; i < TRIES; i++) {
      await sleep(STEP)
      const now = await read()
      if (!now || !sameChoices(now, menu)) return true
      if (done(now)) return true
    }
    return false
  }

  const run = async (b: NonNullable<Busy>, fn: () => Promise<string | null>) => {
    if (busy) return
    setBusy(b)
    setNote(null)
    try {
      setNote(await fn())
    } catch {
      setNote(t('按鍵沒有送出去，請查看終端位置後再試。'))
    } finally {
      setBusy(null)
      onAnswered?.()
    }
  }

  const newPin = () => runPin(() => readTerminal(botId, 'visible', 200), () => useStore.getState().runs[botId]?.id)
  const goneMsg = t('這次作答的 Run 已經換了（bot 重啟過）或讀不到畫面，什麼都沒送出。')
  const asyncGuard = async (pin: RunPin, i: number, typed?: string): Promise<boolean> => {
    const raw = await pin.read()
    const now = raw === null ? null : parseChoiceMenu(raw)
    return Boolean(
      menu.question &&
        raw !== null &&
        now?.asyncQueue &&
        now.question === menu.question &&
        now.cursor === i &&
        now.choices.length === menu.choices.length &&
        now.choices.every((c, j) =>
          typed !== undefined && j === i ? squashText(c.title) === squashText(typed) : c.title === menu.choices[j].title,
        ) &&
        panelShows(raw, menu.question),
    )
  }

  const pickAsync = (i: number) =>
    run({ kind: 'choice', key: String(i) }, async () => {
      const pin = newPin()
      const raw = await pin.read()
      if (raw === null) return goneMsg
      const now = parseChoiceMenu(raw)
      if (!stillHere(now)) return t('畫面在這中間換過了，剛剛那一下沒有送出去——上面是重讀後的選單，請再點一次。')
      const move = keysToMove(now, i)
      if (!move) return t('游標走不到這一項（畫面可能換了），什麼都沒送出。')
      if (move.length) await sendKeys(botId, move, pin.id())
      let there = false
      for (let t = 0; t < TRIES && !there; t++) {
        if (move.length) await sleep(STEP)
        there = await asyncGuard(pin, i)
        if (pin.changed()) return goneMsg
      }
      if (!there || !(await asyncGuard(pin, i))) return pin.changed() ? goneMsg : t('移動後畫面不是同一題、游標不在這一項或有選項變了，沒有按 Enter；請查看終端。')
      await sendKeys(botId, ['enter'], pin.id())
      return null
    })

  const typeAsync = (i: number) =>
    run({ kind: 'choice', key: String(i) }, async () => {
      const text = typeText.trim()
      if (!text) return t('請先打字，或改選別項。')
      const pin = newPin()
      const raw = await pin.read()
      if (raw === null) return goneMsg
      const first = parseChoiceMenu(raw)
      if (!stillHere(first)) return t('畫面在這中間換過了，什麼都沒送出；請再點一次。')
      let at = first
      for (let attempt = 0; attempt < WALK_TRIES && at.cursor !== i; attempt++) {
        const move = keysToMove(at, i)
        if (!move?.length) return t('游標走不到這一項（畫面可能換了），什麼都沒送出。')
        await sendKeys(botId, move, pin.id())
        await sleep(STEP)
        const next = await pin.read()
        const parsed = next === null ? null : parseChoiceMenu(next)
        if (!parsed || !stillHere(parsed)) return pin.changed() ? goneMsg : t('畫面在這中間換過了，什麼都沒送出。')
        at = parsed
      }
      if (!(await asyncGuard(pin, i))) return pin.changed() ? goneMsg : t('畫面已經不是這一題的選單（可能換題或切回主輸入框了），什麼都沒送出。')
      if (!(await sendText(botId, text, false, false, pin.id()))) return t('文字沒有送出去。')
      let shown = false
      for (let attempt = 0; attempt < TRIES && !shown; attempt++) {
        await sleep(STEP)
        shown = await asyncGuard(pin, i, text)
        if (pin.changed()) return goneMsg
      }
      if (!shown || !(await asyncGuard(pin, i, text))) return pin.changed() ? goneMsg : t('那一列的內容不等於要送的答案（或換題、游標不在這一列），沒有按 Enter；請查看終端。')
      await sendKeys(botId, ['enter'], pin.id())
      return null
    })

  const pickOne = (i: number) =>
    run({ kind: 'choice', key: String(i) }, async () => {
      const now = await read()
      if (!stillHere(now)) return t('畫面在這中間換過了，剛剛那一下沒有送出去——上面是重讀後的選單，請再點一次。')
      await sendKeys(botId, keysToSelect(now, i))
      if (EDIT_PROMPT_CHOICE.test(menu.choices[i]?.title ?? '')) void loadComposerDraft(botId, { editNow: true, tries: 8, delayMs: 500 })
      return null
    })

  const sendTyped = (i: number) =>
    run({ kind: 'choice', key: String(i) }, async () => {
      const text = typeText.trim()
      if (!text) return t('請先打字，或改選別項。')
      const landed = await walkTo(i)
      if (!landed) return t('游標走不到這一項（畫面可能換了），什麼都沒送出。')
      const ok = await sendText(botId, text, false)
      if (!ok) return t('文字沒有送出去。')
      let shown = false
      for (let t = 0; t < TRIES; t++) {
        await sleep(STEP)
        const now = await read()
        if (now?.choices[i] && customAnswerShown(now.choices[i], text)) {
          shown = true
          break
        }
      }
      if (!shown) return t('自訂文字沒有出現在畫面上，沒有按 Enter。')
      if (menu.multi) {
        const now = await read()
        if (!(now?.choices[i] && customAnswerChecked(now.choices[i], text))) return t('自訂文字出現了但那列沒勾起來，沒有再按鍵；請看終端。')
        setTypeFor(null)
        return null
      }
      await sendKeys(botId, ['enter'])
      return null
    })

  const toggle = (i: number) =>
    run({ kind: 'choice', key: String(i) }, async () => {
      const landed = await walkTo(i)
      if (!landed) return t('游標走不到這一項（畫面可能換了），什麼都沒送出。')
      const before = landed.choices[i].checked
      const flipped = (now: TuiChoiceMenu) => now.choices[i]?.checked !== before
      if (await pressUntil('space', flipped)) return null
      const n = menu.choices[i].number
      if (n <= 9 && (await pressUntil(String(n), flipped))) return null
      return t('這個選單不吃點選（space 與數字鍵都沒反應），請用下面的按鍵。')
    })

  const activate = (i: number) => {
    if (typedAnswerHere(menu, menu.choices[i]) && !(menu.multi && menu.choices[i].checked)) {
      setTypeFor(i)
      setNote(null)
      return
    }
    if (menu.asyncQueue) return pickAsync(i)
    return menu.multi && menu.choices[i].checked !== null ? toggle(i) : pickOne(i)
  }

  const submit = () =>
    run({ kind: 'submit', key: 'submit' }, async () => {
      const landed = await walkTo('submit')
      if (!landed?.submit?.current) return t('游標沒有停在 Submit 上，沒有送出。')
      await sendKeys(botId, ['enter'])
      return null
    })

  const moveTabs = async (delta: number): Promise<boolean> => {
    const before = await read()
    if (!before) return false
    const n = Math.abs(delta)
    for (const key of delta > 0 ? ['right', 'tab'] : ['left', 'shift+tab']) {
      await sendKeys(
        botId,
        Array.from({ length: n }, () => key),
      )
      for (let i = 0; i < TRIES; i++) {
        await sleep(STEP)
        const now = await read()
        if (!now || !sameChoices(now, before) || now.tabAt !== before.tabAt) return true
      }
    }
    return false
  }

  const goTab = (to: number) =>
    run({ kind: 'tab', key: String(to) }, async () => {
      if (atTab === null) return t('看不出現在在第幾題，請用左右那兩顆一格一格走。')
      if (to === atTab) return null
      if (await moveTabs(to - atTab)) {
        setTabAt(to)
        return null
      }
      return t('分頁沒有換——現在可能不在我猜的那一題上，請用左右那兩顆一格一格走。')
    })

  const step = (dir: 1 | -1) =>
    run({ kind: 'tab', key: dir > 0 ? 'next' : 'prev' }, async () => {
      if (await moveTabs(dir)) {
        if (atTab !== null) setTabAt(Math.min(Math.max(atTab + dir, 0), menu.tabs.length - 1))
        return null
      }
      return t('分頁沒有換。')
    })

  const warnFrom = (() => {
    const i = menu.context.findIndex((l) => /^\s*(dangerous|warning|⚠)/i.test(l))
    return i < 0 ? Number.POSITIVE_INFINITY : i
  })()

  const previewAt = menu.choices.findIndex((c) => c.current && c.preview !== '')

  const reviewTab = (i: number) => menu.tabs.findIndex((t, k) => !t.submit && menu.tabs.slice(0, k).filter((x) => !x.submit).length === i)

  return (
    <div className="blocked-choices">
      {}
      {menu.command.length ? (
        <div className="bc-command">
          <pre className="bc-context bc-cmd">{menu.command.join('\n')}</pre>
          {menu.commandCut ? (
            <p className="bc-cmd-cut" role="note">
              {t('指令很長、pane 太矮，上半段已捲出終端畫面：這裡只有看得到的後半段。完整指令看 agent 的對話紀錄。')}
            </p>
          ) : null}
        </div>
      ) : null}
      <div className="bc-top">
        {menu.tabs.length ? (
          <div className="bc-tabs" role="group" aria-label={t('這份問卷的各題')}>
            <button
              type="button"
              className="bc-step"
              aria-label={t('上一題')}
              title={t('上一題（送 shift+tab）')}
              disabled={Boolean(busy)}
              onClick={() => void step(-1)}
            >
              ←
            </button>
            {menu.tabs.map((tab, i) => (
              <button
                key={tab.label}
                type="button"
                className={`bc-tab${i === atTab ? ' bc-tab-at' : ''}${tab.done ? ' bc-tab-done' : ''}`}
                aria-current={i === atTab ? 'true' : undefined}
                disabled={Boolean(busy)}
                title={
                  i === atTab
                    ? t('應該就是現在這一題（終端只用顏色標，快照讀不出來，這是推測）')
                    : t('切到「{label}」{done}', { label: tab.label, done: tab.done ? t('（已經答過）') : '' })
                }
                onClick={() => void goTab(i)}
              >
                <span aria-hidden="true">{tab.submit ? '✔' : tab.done ? '☑' : '☐'}</span>
                {i === atTab && disp?.header ? disp.header : tab.label}
              </button>
            ))}
            <button
              type="button"
              className="bc-step"
              aria-label={t('下一題')}
              title={t('下一題（送 tab）')}
              disabled={Boolean(busy)}
              onClick={() => void step(1)}
            >
              →
            </button>
          </div>
        ) : pendingAt && pendingAt.total > 1 ? (
          <div className="bc-tabs" role="group" aria-label={t('這份問卷的各題')}>
            <button
              type="button"
              className="bc-step"
              aria-label={t('上一題')}
              title={t('上一題（送 shift+tab）')}
              disabled={Boolean(busy) || pendingAt.at === 0}
              onClick={() => void step(-1)}
            >
              ←
            </button>
            <span className="bc-tab bc-tab-at bc-tab-count" aria-current="true">
              {t('第 {current}／{total} 題', { current: pendingAt.at + 1, total: pendingAt.total })}{pendingAt.header ? ` · ${pendingAt.header}` : ''}
            </span>
            <button
              type="button"
              className="bc-step"
              aria-label={t('下一題')}
              title={t('下一題（送 tab）')}
              disabled={Boolean(busy)}
              onClick={() => void step(1)}
            >
              →
            </button>
          </div>
        ) : null}
        {}
        {menu.context.length ? (
          <pre className="bc-context bc-notes">
            {menu.context.map((l, i) => (
              <span key={`${i}-${l}`} className={i >= warnFrom ? 'bc-ctx-warn' : undefined}>
                {l}
                {'\n'}
              </span>
            ))}
          </pre>
        ) : null}
        {menu.question ? (
          <p className="bc-question">{disp?.question ?? menu.question}</p>
        ) : pendingAt ? (
          <p className="bc-question" title={t('終端沒畫出題目，這是對話紀錄裡的原題')}>
            {disp?.question ?? pendingAt.question}
          </p>
        ) : null}
        {tr.button ? <div className="bc-tr">{tr.button}</div> : null}
        {}
        {menu.notes.length ? <pre className="bc-context bc-notes">{menu.notes.join('\n')}</pre> : null}
      </div>

      {}
      {menu.review.length ? (
        <ul className="bc-review">
          {menu.review.map((r, i) => (
            <li key={r.question}>
              <button
                type="button"
                className="bc-rev"
                disabled={Boolean(busy) || reviewTab(i) < 0}
                title={t('回到「{question}」改答案', { question: menu.tabs[reviewTab(i)]?.label ?? t('這一題') })}
                onClick={() => void goTab(reviewTab(i))}
              >
                <span className="bc-rev-q">{r.question}</span>
                <span className="bc-rev-a">{r.answer || t('（還沒作答）')}</span>
              </button>
            </li>
          ))}
        </ul>
      ) : null}

      <ol
        className={`bc-list${menu.multi ? ' bc-list-multi' : ' bc-list-radio'}`}
        role={menu.multi ? undefined : 'radiogroup'}
      >
        {menu.choices.map((c, i) => {
          const shown = open.includes(i)
          const typeOpen = typeFor === i && typedAnswerHere(menu, c)
          return (
          <li key={`${c.number}-${c.title}`} className={`bc-row${c.detail ? ' bc-row-more' : ''}`}>
            <button
              type="button"
              role={menu.multi ? undefined : 'radio'}
              className={`bc-item${c.current ? ' bc-current' : ''}${c.checked || typeOpen ? ' bc-checked' : ''}${
                shown ? ' bc-open' : ''
              }`}
              disabled={Boolean(busy)}
              aria-current={c.current ? 'true' : undefined}
              aria-checked={menu.multi ? undefined : c.current}
              aria-pressed={c.checked === null ? undefined : c.checked}
              title={
                c.checked === null
                    ? t('選這一項（游標移過去再按 Enter）')
                    : c.checked
                      ? t('取消勾選（游標移過去再按 space）')
                      : t('勾選這一項（游標移過去再按 space），勾完按下面的「送出」')
              }
              onClick={() => void activate(i)}
            >
              <span className="bc-num" aria-hidden="true">
                {c.number}
              </span>
              {}
              <span className="bc-box" aria-hidden="true">
                {menu.multi
                  ? c.checked === null
                    ? ''
                    : c.checked
                      ? '☑'
                      : '☐'
                  : c.current
                    ? '●'
                    : '○'}
              </span>
              <span className="bc-title">{disp?.titles[i] ?? c.title}</span>
              <span className="bc-mark">
                {jev && jevMatchesTitle(jev.label, c.title) ? (
                  <JevLabel className="bc-jev" prob={jev.score} />
                ) : null}
                {busy?.kind === 'choice' && busy.key === String(i) ? t('送出中…') : c.current ? t('游標在此') : ''}
              </span>
              {shown && c.detail ? <span className="bc-detail">{disp?.details[i] ?? c.detail}</span> : null}
            </button>
            {}
            <button
                type="button"
                className="bc-chev"
                aria-expanded={shown}
                aria-label={shown ? t('收起第 {n} 項的說明', { n: c.number }) : t('看第 {n} 項的說明', { n: c.number })}
                title={shown ? t('收起說明') : t('看這一項的說明')}
                onClick={(e) => toggleDetail(i, e.currentTarget.parentElement as HTMLElement)}
              >
                <span aria-hidden="true">{shown ? '▾' : '▸'}</span>
              </button>
              {typeOpen ? (
                <>
                  <TypeAnswerField value={typeText} onChange={setTypeText} disabled={Boolean(busy)} />
                  <button
                    type="button"
                    className="bc-submit bc-type-send"
                    disabled={Boolean(busy) || !typeText.trim()}
                    onClick={() => void (menu.asyncQueue ? typeAsync(i) : sendTyped(i))}
                  >
                    {busy?.kind === 'choice' && busy.key === String(i) ? t('送出中…') : t('用這段作答')}
                  </button>
                </>
              ) : null}
          </li>
          )
        })}
      </ol>
      {jevRowShown ? <p className="bc-jev-note">{jevAskNote()}</p> : null}

      {}
      {previewAt >= 0 ? (
        <figure className="bc-preview">
          <figcaption>
            {t('第 {n} 項的預覽（終端只畫游標所在那一項；移動游標才看得到別項的）', { n: menu.choices[previewAt].number })}
          </figcaption>
          <pre className="bc-context bc-notes bc-preview-text">{menu.choices[previewAt].preview}</pre>
        </figure>
      ) : null}

      {menu.submit ? (
        <button type="button" className="bc-submit" disabled={Boolean(busy)} onClick={() => void submit()}>
          {busy?.kind === 'submit' ? t('送出中…') : t('送出（Submit）')}
        </button>
      ) : null}

      {}
      {note ? (
        <p className="bc-hint bc-stale" role="status">
          {note}
        </p>
      ) : null}
    </div>
  )
}

export function BlockedExtrasBar({
  botId,
  open,
  onToggle,
  onAnswered,
}: {
  botId: string
  open: boolean
  onToggle: () => void
  onAnswered?: () => void
}) {
  const t = useT()
  const press = usePaneKeys(botId, onAnswered)
  return (
    <div className="bc-bar">
      <button type="button" className="bc-esc" title={t('送出 Esc：取消這個問題')} onClick={() => press(['esc'])}>
        {t('Esc 取消')}
      </button>
      <button type="button" className="mini-btn bc-more" aria-expanded={open} onClick={onToggle}>
        {open ? t('收起終端原文') : t('終端原文與更多按鍵')}
      </button>
    </div>
  )
}
