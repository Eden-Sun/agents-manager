import test from 'node:test'
import assert from 'node:assert/strict'
import { commit, pageNeedsCommit, preload, radioPick, samePage, togglesFor, wantOf, type Io } from './choiceDraft.ts'
import { parseChoiceMenu } from './tuiChoices.ts'

/**
 * 一台假的 claude 問卷 TUI：三個分頁（兩題複選 ＋ 一個送出頁），會對 ←／→、↑／↓、space、
 * 數字鍵與 Enter 有反應，而且**畫出來的字跟真機快照同一個版型**（`carbis-multiselect.txt`
 * 的縮排、沒有編號的 `Submit` 列、夾在中間的分隔線都照抄）。
 *
 * 有它才驗得了「預載走一輪回到原點」「差集只送要翻的那幾顆」「對不上就停手」這幾條——真的
 * bot 上那題是使用者本人要回答的，不能拿來按。
 */
class FakeTui {
  tab = 0
  cursor = 0
  /** 每頁的勾選狀態。 */
  checked = [
    [false, false, false],
    [false, false, false],
  ]
  /** 收到過的鍵，順序原樣。 */
  sent: string[] = []
  submitted = false
  /** 每頁按過自己的 Submit 沒有。 */
  pageDone = [false, false]

  private readonly pages = [
    { label: '編輯方式', q: '文章是誰寫、怎麼上稿？', opts: ['非工程師要能自己發', '工程師改 Markdown', '兩種都要'] },
    { label: '功能', q: '除了看文章，還需要哪些功能？', opts: ['站內搜尋、分類、標籤', '訂閱電子報', 'SEO 是主要目的'] },
  ]

  /** 這一頁游標走得到的列：3 個選項 ＋ `Type something` ＋ `Submit`。 */
  private rows(): number {
    return this.tab < 2 ? this.pages[this.tab].opts.length + 2 : 2
  }

  private tabBar(): string {
    const cells = [
      `${this.pageDone[0] ? '☒' : '☐'} 編輯方式`,
      `${this.pageDone[1] ? '☒' : '☐'} 功能`,
      '✔ Submit',
    ]
    return `←  ${cells.join('  ')}  →`
  }

  screen(): string {
    const mark = (i: number) => (this.cursor === i ? '❯ ' : '  ')
    const out = ['前面一堆別的輸出', '─'.repeat(60), this.tabBar(), '']
    if (this.tab === 2) {
      out.push('Review your answers', '')
      this.pages.forEach((p, i) => {
        const picked = p.opts.filter((_, j) => this.checked[i][j])
        out.push(` ● ${p.q}`, `   → ${picked.join('、') || '（沒選）'}`)
      })
      out.push('', 'Ready to submit your answers?', '', `${mark(0)}1. Submit answers`, `${mark(1)}2. Cancel`)
      return out.join('\n')
    }
    const p = this.pages[this.tab]
    out.push(p.q, '')
    p.opts.forEach((o, i) => out.push(`${mark(i)}${i + 1}. [${this.checked[this.tab][i] ? 'x' : ' '}] ${o}`))
    out.push(`${mark(p.opts.length)}${p.opts.length + 1}. [ ] Type something`)
    out.push(`${this.cursor === p.opts.length + 1 ? '❯ ' : '     '}Submit`)
    out.push('', 'Enter to select · Tab/Arrow keys to navigate · Esc to cancel')
    return out.join('\n')
  }

  key(k: string) {
    this.sent.push(k)
    if (k === 'right' || k === 'tab') {
      if (this.tab < 2) {
        this.tab += 1
        this.cursor = 0
      }
      return
    }
    if (k === 'left' || k === 'shift+tab') {
      if (this.tab > 0) {
        this.tab -= 1
        this.cursor = 0
      }
      return
    }
    if (k === 'down') {
      this.cursor = Math.min(this.cursor + 1, this.rows() - 1)
      return
    }
    if (k === 'up') {
      this.cursor = Math.max(this.cursor - 1, 0)
      return
    }
    if (this.tab === 2) {
      if (k === 'enter' && this.cursor === 0) this.submitted = true
      return
    }
    const opts = this.pages[this.tab].opts.length
    if (k === 'space' && this.cursor < opts) {
      this.checked[this.tab][this.cursor] = !this.checked[this.tab][this.cursor]
      return
    }
    if (/^[1-9]$/.test(k)) {
      const i = Number(k) - 1
      if (i < opts) this.checked[this.tab][i] = !this.checked[this.tab][i]
      return
    }
    // 這一頁的 `Submit` 列：按 Enter 才算答完這一題。
    if (k === 'enter' && this.cursor === opts + 1) this.pageDone[this.tab] = true
  }

  io(): Io {
    return {
      read: async () => parseChoiceMenu(this.screen()),
      send: async (keys) => keys.forEach((k) => this.key(k)),
      wait: async () => {},
      paste: async () => {},
    }
  }
}

test('假 TUI 自己畫得出來的畫面，parseChoiceMenu 認得（三個分頁、複選、Submit 列）', () => {
  const tui = new FakeTui()
  const m = parseChoiceMenu(tui.screen())
  assert.ok(m)
  assert.equal(m.tabs.length, 3)
  assert.equal(m.multi, true)
  assert.equal(m.choices.length, 4)
  assert.ok(m.submit)
})

test('預載把三頁都讀回來，而且走完停回原本那一頁', async () => {
  const tui = new FakeTui()
  tui.tab = 1
  tui.pageDone[0] = true // 第一頁答過了 → tabAt 猜第二頁
  const start = parseChoiceMenu(tui.screen())
  assert.ok(start)
  assert.equal(start.tabAt, 1)

  const steps: number[] = []
  const draft = await preload(tui.io(), start, (d) => steps.push(d))
  assert.ok(draft)
  assert.equal(draft.pages.length, 3)
  assert.deepEqual(
    draft.pages.map((p) => p.label),
    ['編輯方式', '功能', 'Submit'],
  )
  assert.equal(draft.pages[2].isSubmit, true)
  assert.equal(draft.pages[0].choices[0].title, '非工程師要能自己發')
  assert.deepEqual(steps, [1, 2, 3])
  // 回到起點，而且全程沒有送任何會改到答案的鍵
  assert.equal(tui.tab, 1)
  assert.deepEqual(tui.checked, [
    [false, false, false],
    [false, false, false],
  ])
  assert.equal(
    tui.sent.every((k) => ['left', 'right', 'tab', 'shift+tab'].includes(k)),
    true,
  )
})

test('走不動就整個放棄，不留讀了一半的草稿', async () => {
  const tui = new FakeTui()
  const start = parseChoiceMenu(tui.screen())
  assert.ok(start)
  const io = tui.io()
  // 導覽鍵全部吃掉 → 畫面永遠不變
  const dead: Io = { ...io, send: async () => {} }
  assert.equal(await preload(dead, start), null)
})

test('差集：只翻要翻的那幾顆，已經對的不動', () => {
  const now = parseChoiceMenu(new FakeTui().screen())!.choices
  assert.deepEqual(togglesFor(now, [true, false, true]), [0, 2])
  assert.deepEqual(togglesFor(now, [false, false, false]), [])
  // 沒有方框的那一列（Type something）不在差集裡——那是動作不是勾選
  assert.equal(togglesFor(now, [true, true, true, true]).includes(3), true)
})

test('一次送出：每頁只送差集、按過該頁的 Submit，最後停在 Submit answers', async () => {
  const tui = new FakeTui()
  tui.checked[0] = [true, false, false] // 第一頁已經有一個勾
  const start = parseChoiceMenu(tui.screen())
  assert.ok(start)
  const draft = await preload(tui.io(), start)
  assert.ok(draft)

  // 使用者要的：第一頁換成第 3 項、第二頁勾第 1 與第 3 項
  const want = [
    [false, false, true],
    [true, false, true],
    [],
  ]
  tui.sent = []
  const res = await commit(tui.io(), draft, want)
  assert.deepEqual(res, { ok: true })
  assert.deepEqual(tui.checked, [
    [false, false, true],
    [true, false, true],
  ])
  assert.deepEqual(tui.pageDone, [true, true])
  assert.equal(tui.submitted, true)
  // 第一頁只翻了兩顆（1 取消、3 勾上），不是把三顆都送一遍
  assert.equal(tui.sent.filter((k) => k === 'space').length, 4)
})

test('沒改到的那一頁不會被走過去亂按', async () => {
  const tui = new FakeTui()
  const start = parseChoiceMenu(tui.screen())
  assert.ok(start)
  const draft = await preload(tui.io(), start)
  assert.ok(draft)
  tui.sent = []
  const res = await commit(tui.io(), draft, [[], [false, true, false], []])
  assert.equal(res.ok, true)
  assert.deepEqual(tui.checked[0], [false, false, false])
  assert.deepEqual(tui.pageDone, [false, true])
})

test('送出前畫面換掉：整批不送，回一句看得懂的話', async () => {
  const tui = new FakeTui()
  const start = parseChoiceMenu(tui.screen())
  assert.ok(start)
  const draft = await preload(tui.io(), start)
  assert.ok(draft)
  // 把草稿的第二頁題目換掉，模擬「終端上已經不是同一份問卷」
  draft.pages[1] = { ...draft.pages[1], question: '這題早就換掉了' }
  const res = await commit(tui.io(), draft, [[], [true, false, false], []])
  assert.equal(res.ok, false)
  assert.match(res.error ?? '', /跟讀進來時不一樣|沒對上|不一樣/)
  assert.equal(tui.submitted, false)
})

test('按不動就停在那裡，不會繼續送後面的鍵', async () => {
  const tui = new FakeTui()
  const start = parseChoiceMenu(tui.screen())
  assert.ok(start)
  const draft = await preload(tui.io(), start)
  assert.ok(draft)
  const io = tui.io()
  // space 與數字鍵都被吃掉：只剩導覽走得動
  const numb: Io = {
    ...io,
    send: async (keys) => {
      for (const k of keys) if (!/^(space|[1-9])$/.test(k)) tui.key(k)
    },
  }
  const res = await commit(numb, draft, [[true, false, false], [], []])
  assert.equal(res.ok, false)
  assert.match(res.error ?? '', /按不動/)
  assert.equal(tui.submitted, false)
  assert.deepEqual(tui.checked[0], [false, false, false])
})

/**
 * 多分頁單選：沒有 `[ ]`、沒有每頁的 Submit 列。Enter 選一項；送出頁才有 Submit answers。
 * 版型照 `opu-radio.txt`（真機 0-opu 那份，題目改成假的，不拿真人問卷來按）。
 */
class FakeRadioTui {
  tab = 0
  cursor = 0
  chosen: (number | null)[] = [null, null, null, null]
  typed: string[] = ['', '', '', '']
  sent: string[] = []
  pasted: string[] = []
  submitted = false

  private readonly pages = [
    { label: '換身分門檻', q: 'cc2 撞到哪一種才換下一個身分？', opts: ['7d 或 limit_hit 才換', '任一桶撞限就換', '只看 7d'] },
    { label: 'Fable 用完', q: 'Fable 用完怎麼辦？', opts: ['切 opus', '換身分', '停住等'] },
    { label: '執行者 kind', q: '執行者用哪一種？', opts: ['跟 PM 同一 kind', '固定 grok', '隨便'] },
    { label: '推 main 失敗', q: 'push 被拒怎麼辦？', opts: ['rebase 再推', '開 PR', '停住'] },
  ]

  private rows(): number {
    return this.tab < 4 ? this.pages[this.tab].opts.length + 2 : 2
  }

  private tabBar(): string {
    const cells = this.pages.map((p, i) => `${this.chosen[i] !== null ? '☒' : '☐'} ${p.label}`)
    cells.push('✔ Submit')
    return `←  ${cells.join('  ')}  →`
  }

  screen(): string {
    const mark = (i: number) => (this.cursor === i ? '❯ ' : '  ')
    const out = ['前面一堆別的輸出', '─'.repeat(60), this.tabBar(), '']
    if (this.tab === 4) {
      out.push('Review your answers', '')
      this.pages.forEach((p, i) => {
        let a = '（沒選）'
        if (this.chosen[i] === p.opts.length) a = this.typed[i] || 'Type something.'
        else if (this.chosen[i] !== null) a = p.opts[this.chosen[i]!] ?? '（沒選）'
        const lines = a.split('\n')
        out.push(` ● ${p.q}`, `   → ${lines[0]}`)
        for (const extra of lines.slice(1)) out.push(`   ${extra}`)
      })
      out.push('', 'Ready to submit your answers?', '', `${mark(0)}1. Submit answers`, `${mark(1)}2. Cancel`)
      return out.join('\n')
    }
    const p = this.pages[this.tab]
    out.push(p.q, '')
    p.opts.forEach((o, i) => out.push(`${mark(i)}${i + 1}. ${o}`))
    const typeIdx = p.opts.length
    const typed = this.typed[this.tab]
    if (typed) {
      const lines = typed.split('\n')
      const tick = this.chosen[this.tab] === typeIdx ? ' ✔' : ''
      out.push(`${mark(typeIdx)}${typeIdx + 1}. ${lines[0]}${tick}`)
      for (const extra of lines.slice(1)) out.push(`     ${extra}`)
    } else {
      out.push(`${mark(typeIdx)}${typeIdx + 1}. Type something.`)
    }
    out.push('─'.repeat(60))
    out.push(`${mark(p.opts.length + 1)}${p.opts.length + 2}. Chat about this`)
    out.push('Enter to select · Tab/Arrow keys to navigate · Esc to cancel')
    return out.join('\n')
  }

  key(k: string) {
    this.sent.push(k)
    if (k === 'right' || k === 'tab') {
      if (this.tab < 4) {
        this.tab += 1
        this.cursor = 0
      }
      return
    }
    if (k === 'left' || k === 'shift+tab') {
      if (this.tab > 0) {
        this.tab -= 1
        this.cursor = 0
      }
      return
    }
    if (k === 'down') {
      this.cursor = Math.min(this.cursor + 1, this.rows() - 1)
      return
    }
    if (k === 'up') {
      this.cursor = Math.max(this.cursor - 1, 0)
      return
    }
    if (this.tab === 4) {
      if (k === 'enter' && this.cursor === 0) this.submitted = true
      return
    }
    if (k === 'enter') this.chosen[this.tab] = this.cursor
  }

  paste(text: string) {
    this.pasted.push(text)
    if (this.tab < 4 && this.cursor === this.pages[this.tab].opts.length) this.typed[this.tab] = text
  }

  io(): Io {
    return {
      read: async () => parseChoiceMenu(this.screen()),
      send: async (keys) => keys.forEach((k) => this.key(k)),
      wait: async () => {},
      paste: async (text) => this.paste(text),
    }
  }
}

test('假單選 TUI 畫得出來的畫面，parseChoiceMenu 認成單選多分頁', () => {
  const tui = new FakeRadioTui()
  const m = parseChoiceMenu(tui.screen())
  assert.ok(m)
  assert.equal(m.multi, false)
  assert.equal(m.tabs.length, 5)
  assert.equal(m.choices.length, 5)
  assert.equal(m.choices[0].checked, null)
  assert.equal(m.submit, null)
})

test('單選預載五頁走完停回起點，全程不送 Enter', async () => {
  const tui = new FakeRadioTui()
  const start = parseChoiceMenu(tui.screen())
  assert.ok(start)
  const draft = await preload(tui.io(), start)
  assert.ok(draft)
  assert.equal(draft.pages.length, 5)
  assert.equal(draft.pages[4].isSubmit, true)
  assert.equal(draft.pages.every((p, i) => i === 4 || p.multi === false), true)
  assert.deepEqual(wantOf(draft.pages[0]), [false, false, false, false, false])
  assert.equal(pageNeedsCommit(draft.pages[0], wantOf(draft.pages[0])), false)
  assert.equal(tui.tab, 0)
  assert.equal(tui.sent.includes('enter'), false)
  assert.deepEqual(tui.chosen, [null, null, null, null])
})

test('單選一次送出：每頁 Enter 那一項，最後 Submit answers；不送 space', async () => {
  const tui = new FakeRadioTui()
  const start = parseChoiceMenu(tui.screen())
  assert.ok(start)
  const draft = await preload(tui.io(), start)
  assert.ok(draft)

  const want = [
    [true, false, false, false, false],
    [false, true, false, false, false],
    [false, false, true, false, false],
    [true, false, false, false, false],
    [],
  ]
  assert.equal(radioPick(want[1]), 1)
  tui.sent = []
  const res = await commit(tui.io(), draft, want)
  assert.deepEqual(res, { ok: true })
  assert.deepEqual(tui.chosen, [0, 1, 2, 0])
  assert.equal(tui.submitted, true)
  assert.equal(tui.sent.includes('space'), false)
  assert.ok(tui.sent.filter((k) => k === 'enter').length >= 5)
})

test('Type something 空字串不能送，終端一顆鍵都沒動', async () => {
  const tui = new FakeRadioTui()
  const start = parseChoiceMenu(tui.screen())
  assert.ok(start)
  const draft = await preload(tui.io(), start)
  assert.ok(draft)
  const want = [[false, false, false, true, false], [], [], [], []]
  tui.sent = []
  const res = await commit(tui.io(), draft, want, undefined, ['', '', '', '', ''])
  assert.equal(res.ok, false)
  assert.match(res.error ?? '', /沒有打字/)
  assert.equal(tui.sent.length, 0)
  assert.equal(tui.submitted, false)
})

test('Type something：預載 → 貼中文多行 → Enter → 對帳出現在 review', async () => {
  const tui = new FakeRadioTui()
  const start = parseChoiceMenu(tui.screen())
  assert.ok(start)
  const draft = await preload(tui.io(), start)
  assert.ok(draft)
  const want = [
    [false, false, false, true, false],
    [true, false, false, false, false],
    [true, false, false, false, false],
    [true, false, false, false, false],
    [],
  ]
  const custom = ['第一行\n第二行中文', '', '', '', '']
  tui.sent = []
  const res = await commit(tui.io(), draft, want, undefined, custom)
  assert.deepEqual(res, { ok: true })
  assert.equal(tui.typed[0], '第一行\n第二行中文')
  assert.deepEqual(tui.chosen, [3, 0, 0, 0])
  assert.equal(tui.submitted, true)
  assert.deepEqual(tui.pasted, ['第一行\n第二行中文'])
  const review = parseChoiceMenu(tui.screen())
  assert.ok(review)
  assert.ok(review.review[0]?.answer.includes('第一行'))
  assert.ok(review.review[0]?.answer.includes('第二行中文'))
})

test('wantOf 拿讀到當下的勾選當預設；samePage 只比題目與選項字串', async () => {
  const tui = new FakeTui()
  tui.checked[0] = [false, true, false]
  const start = parseChoiceMenu(tui.screen())
  assert.ok(start)
  const draft = await preload(tui.io(), start)
  assert.ok(draft)
  assert.deepEqual(wantOf(draft.pages[0]), [false, true, false, false])
  // 勾選變了不算「換了一份選單」
  tui.checked[0] = [true, true, true]
  tui.tab = 0
  const now = parseChoiceMenu(tui.screen())
  assert.ok(now)
  assert.equal(samePage(draft.pages[0], now), true)
})
