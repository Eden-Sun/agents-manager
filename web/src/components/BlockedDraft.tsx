/**
 * 多分頁問卷的「先讀完、離線作答、最後一次送出」介面（2026-09-12 第六輪，使用者指定：
 * 「一開始就先預載入所有的分頁，而不是每一次連動，最後送出前再確認就好」）。
 *
 * 認出兩個以上的分頁就先跑一次 [`preload`]：用 ←／→ 把每一頁走過去讀回來，走完回到原本那頁。
 * 之後所有勾選、切題、展開說明都只改本地 state，**完全不碰終端**（點擊是即時的，不再是一次
 * 點擊四到六個 HTTP）。按「送出全部答案」才動終端，而且是照差集只送要翻的那幾顆，一頁一頁
 * 驗過去，任何一步對不上就停手。
 *
 * 預載失敗（走不動、某頁讀不完整）就整個退回即時模式（`BlockedChoices`），不留半套草稿。
 */
import { useEffect, useRef, useState } from 'react'
import { commit, preload, togglesFor, wantOf, type Draft, type Io } from '../lib/choiceDraft'
import { parseChoiceMenu, type TuiChoiceMenu } from '../lib/tuiChoices'
import { useStore } from '../store/store'
import { BlockedChoices } from './BlockedChoices'
import './blockedChoices.css'

type Phase = 'loading' | 'ready' | 'sending' | 'live'

export function BlockedDraft({
  botId,
  menu,
  onAnswered,
}: {
  botId: string
  menu: TuiChoiceMenu
  onAnswered?: () => void
}) {
  const readTerminal = useStore((s) => s.readTerminal)
  const sendKeys = useStore((s) => s.sendKeys)
  const [phase, setPhase] = useState<Phase>('loading')
  const [draft, setDraft] = useState<Draft | null>(null)
  const [want, setWant] = useState<boolean[][]>([])
  const [step, setStep] = useState({ done: 0, total: 0 })
  const [err, setErr] = useState<string | null>(null)
  const [open, setOpen] = useState<string[]>([])
  const [round, setRound] = useState(0)
  const listRef = useRef<HTMLDivElement>(null)
  /** 正在跑（或跑完）的那一份預載，用來擋掉 StrictMode 的第二次 effect。 */
  const jobRef = useRef<{ round: number; job: Promise<Draft | null> } | null>(null)

  // 手上這份選單的身分：問卷換了（分頁的標籤組合不一樣）就重新預載。
  const ident = menu.tabs.map((t) => t.label).join('')
  const identRef = useRef(ident)

  const io: Io = {
    // 讀取量收到 60 行：選單只佔畫面下半部，200 行每次都在搬一大包 JSON（第五輪 B）。
    read: async () => {
      try {
        return parseChoiceMenu((await readTerminal(botId, 'visible', 60)).text)
      } catch {
        return null
      }
    },
    send: (keys) => sendKeys(botId, keys),
    wait: (ms) => new Promise((r) => setTimeout(r, ms)),
  }
  const ioRef = useRef(io)
  ioRef.current = io

  useEffect(() => {
    if (identRef.current !== ident) {
      identRef.current = ident
      setPhase('loading')
      setDraft(null)
      setRound((n) => n + 1)
    }
  }, [ident])

  /**
   * 預載。`round` 變了（換問卷、或使用者按「重新讀取」）就再跑一次。
   *
   * **同一輪只准跑一份**：StrictMode 在 dev 會把 effect 跑兩次（mount → cleanup → mount），
   * 兩份預載同時在同一個 pane 上送導覽鍵會互相插隊，走到一半就對不上、還把終端留在別的分頁。
   * 所以把那顆 promise 記在 ref 裡，第二次進來的人接同一份結果。
   */
  useEffect(() => {
    let alive = true
    setErr(null)
    setStep({ done: 0, total: menu.tabs.length })
    if (jobRef.current?.round !== round) {
      jobRef.current = {
        round,
        job: preload(ioRef.current, menu, (done, total) => setStep({ done, total })),
      }
    }
    void jobRef.current.job.then((d) => {
      if (!alive) return
      if (!d) {
        // 讀不完整就整個退回即時模式，不要送出半套。
        setPhase('live')
        return
      }
      setDraft(d)
      setWant(d.pages.map((p) => wantOf(p)))
      setPhase('ready')
    })
    return () => {
      alive = false
    }
    // menu 每秒都是新物件，不能進 deps；預載只認 round。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [round])

  if (phase === 'live') return <BlockedChoices botId={botId} menu={menu} onAnswered={onAnswered} />

  if (phase === 'loading' || !draft) {
    return (
      <div className="blocked-choices">
        <p className="bc-question">正在讀這份問卷的每一題…</p>
        <p className="bc-hint">
          讀取中 {step.done}/{step.total || menu.tabs.length}
          ——這段只會在分頁之間移動，不會動到任何答案。讀完就可以一次勾完再送出。
        </p>
      </div>
    )
  }

  const dirty = draft.pages.some((p, i) => !p.isSubmit && togglesFor(p.choices, want[i] ?? []).length > 0)
  const busy = phase === 'sending'

  const toggle = (page: number, idx: number) =>
    setWant((v) => v.map((row, i) => (i === page ? row.map((b, j) => (j === idx ? !b : b)) : row)))

  const send = async () => {
    setPhase('sending')
    setErr(null)
    setStep({ done: 0, total: 0 })
    const res = await commit(ioRef.current, draft, want, (done, total) => setStep({ done, total }))
    onAnswered?.()
    if (res.ok) return
    // 失敗就停在那裡，並且把「終端現在到底長什麼樣」重讀回來，不要假裝成功。
    setErr(res.error ?? '送出失敗。')
    setPhase('ready')
  }

  const answerOf = (page: number) =>
    draft.pages[page].choices
      .filter((c, j) => c.checked !== null && want[page]?.[j])
      .map((c) => c.title)
      .join('、')

  const jump = (tab: number) =>
    listRef.current?.querySelector<HTMLElement>(`[data-tab="${tab}"]`)?.scrollIntoView({ block: 'nearest' })

  return (
    <div className="blocked-choices">
      <div className="bc-top">
        <div className="bc-tabs" role="group" aria-label="這份問卷的各題">
          {draft.pages.map((p) => (
            <button
              key={p.tab}
              type="button"
              className={`bc-tab${!p.isSubmit && (want[p.tab] ?? []).some(Boolean) ? ' bc-tab-done' : ''}`}
              disabled={busy}
              title={`跳到「${p.label}」`}
              onClick={() => jump(p.tab)}
            >
              <span aria-hidden="true">{p.isSubmit ? '✔' : (want[p.tab] ?? []).some(Boolean) ? '☑' : '☐'}</span>
              {p.label}
            </button>
          ))}
        </div>
        <p className="bc-question">
          這份問卷有 {draft.pages.filter((p) => !p.isSubmit).length} 題，勾好之後一次送出
        </p>
      </div>

      <div ref={listRef} className="bc-pages">
        {draft.pages.map((page, i) =>
          page.isSubmit ? (
            <section key={page.tab} className="bc-page" data-tab={page.tab}>
              <h4 className="bc-page-q">你的答案</h4>
              <ul className="bc-review">
                {draft.pages
                  .filter((p) => !p.isSubmit)
                  .map((p) => (
                    <li key={p.tab}>
                      <button type="button" className="bc-rev" disabled={busy} onClick={() => jump(p.tab)}>
                        <span className="bc-rev-q">{p.question ?? p.label}</span>
                        <span className="bc-rev-a">{answerOf(p.tab) || '（還沒選）'}</span>
                      </button>
                    </li>
                  ))}
              </ul>
            </section>
          ) : (
            <section key={page.tab} className="bc-page" data-tab={page.tab}>
              <h4 className="bc-page-q">{page.question ?? page.label}</h4>
              <ol className="bc-list bc-list-multi">
                {page.choices.map((c, j) => {
                  const key = `${page.tab}:${j}`
                  const shown = open.includes(key)
                  const picked = c.checked !== null && Boolean(want[i]?.[j])
                  return (
                    <li key={key} className={`bc-row${c.detail ? ' bc-row-more' : ''}`}>
                      <button
                        type="button"
                        className={`bc-item${picked ? ' bc-checked' : ''}${shown ? ' bc-open' : ''}`}
                        disabled={busy || c.checked === null}
                        aria-pressed={c.checked === null ? undefined : picked}
                        title={
                          c.checked === null
                            ? '這一列是動作不是勾選，要用它請先關掉這個畫面回終端'
                            : picked
                              ? '取消勾選（只改這裡，還沒送出）'
                              : '勾選（只改這裡，還沒送出）'
                        }
                        onClick={() => toggle(i, j)}
                      >
                        <span className="bc-num" aria-hidden="true">
                          {c.number}
                        </span>
                        <span className="bc-box" aria-hidden="true">
                          {c.checked === null ? '' : picked ? '☑' : '☐'}
                        </span>
                        <span className="bc-title">{c.title}</span>
                        <span className="bc-mark" />
                        {shown && c.detail ? <span className="bc-detail">{c.detail}</span> : null}
                      </button>
                      <button
                        type="button"
                        className="bc-chev"
                        aria-expanded={shown}
                        aria-label={shown ? '收起說明' : '看這一項的說明'}
                        onClick={() => setOpen((v) => (v.includes(key) ? v.filter((k) => k !== key) : [...v, key]))}
                      >
                        <span aria-hidden="true">{shown ? '▾' : '▸'}</span>
                      </button>
                    </li>
                  )
                })}
              </ol>
            </section>
          ),
        )}
      </div>

      <button type="button" className="bc-submit" disabled={busy} onClick={() => void send()}>
        {busy ? `送出中… ${step.done}/${step.total}` : dirty ? '送出全部答案' : '直接送出（沒有改動）'}
      </button>

      {err ? (
        <p className="bc-hint bc-stale" role="status">
          {err}
          <button type="button" className="mini-btn bc-again" onClick={() => setRound((n) => n + 1)}>
            重新讀取
          </button>
        </p>
      ) : null}
    </div>
  )
}
