import test from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { commit, locate, moveTab, pageNeedsCommit, preload, radioPick, readChoiceMenu, samePage, togglesFor, wantOf, type Draft, type DraftPage, type Io } from './choiceDraft.ts'
import { parseChoiceMenu, typedAnswerHere, type TuiChoiceMenu, type TuiTab } from './tuiChoices.ts'

// #1887：issue 裡的 repo 選項與說明縮排；保留打字列、Submit 與 Chat about this 分隔線。
const REPO_MULTI = [
  '────────────────────────────────────────────────────────────',
  ' ☐ Repo  ✔ Submit',
  '',
  '下一個要 review 哪個 repo？',
  '',
  '❯ 1. [ ] robinstech-carbis-web',
  '         carbis 前台（接 console 報價、評價）',
  '  2. [ ] vehicleapi',
  '         車籍查詢 API',
  '  3. [ ] carmakerapi',
  '         車廠代碼 API',
  '  4. [ ] Type something.',
  '     Submit',
  '────────────────────────────────────────────────────────────',
  '  5. Chat about this',
  '',
  'Enter to select · ↑/↓ to navigate · Esc to cancel',
].join('\n')

test('#1887 上輪回覆的表格不應讓重讀選單變成預覽標題', async () => {
  const full = readFileSync(new URL('./__fixtures__/claude-repo-multiselect-1887.txt', import.meta.url), 'utf8')
  const short = full.slice(full.indexOf('←  ☐ Repo'))
  const menu = parseChoiceMenu(short)!
  const draft = await preload({ read: async () => menu, send: async () => {}, paste: async () => {}, wait: async () => {} }, menu)
  assert.ok(draft)
  const now = await readChoiceMenu(async () => full)
  assert.ok(now)
  assert.equal(samePage(draft.pages[0], now), true)
  assert.deepEqual(now.choices, menu.choices)
  assert.equal(now.choices[1].detail, '車籍查詢 API')
  assert.equal(now.choices[1].title, 'vehicleapi')
})

test('#1887 多選說明分開存放，面板預載與送出重讀仍是同一頁', async () => {
  const menu = parseChoiceMenu(REPO_MULTI)
  assert.ok(menu)
  assert.equal(menu.multi, true)
  assert.deepEqual(menu.choices.map(({ title, detail }) => ({ title, detail })), [
    { title: 'robinstech-carbis-web', detail: 'carbis 前台（接 console 報價、評價）' },
    { title: 'vehicleapi', detail: '車籍查詢 API' },
    { title: 'carmakerapi', detail: '車廠代碼 API' },
    { title: 'Type something.', detail: '' },
    { title: 'Chat about this', detail: '' },
  ])
  assert.deepEqual(menu.submit, { after: 3, current: false })
  const io: Io = {
    read: () => readChoiceMenu(async () => REPO_MULTI),
    send: async () => { assert.fail('單題加合成送出頁預載不應送鍵') },
    paste: async () => { assert.fail('預載不應貼字') },
    wait: async () => {},
  }
  const draft = await preload(io, menu)
  assert.ok(draft)
  const reread = await io.read()
  assert.ok(reread)
  assert.equal(samePage(draft.pages[0], reread), true)
  // 真正換標題仍須拒絕，不能靠刪掉說明或模糊比對放行。
  const changed = parseChoiceMenu(REPO_MULTI.replace('vehicleapi', 'different-api'))
  assert.ok(changed)
  assert.equal(samePage(draft.pages[0], changed), false)
})

/**
 * 假的 claude 問卷 TUI（兩題複選＋送出頁），版型照抄 `carbis-multiselect.txt`。
 * 真 bot 上的題是使用者本人要答的，不能拿來按。
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
  /** 每頁的 `[ ] Type something` 勾了沒；貼字會自己勾起來並把標題換成那段字，space 翻得動它（2026-09-25 真機）。 */
  typeChecked = [false, false]
  typed = ['', '']
  pasted: string[] = []
  typeRowToggles = true

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
    if (this.submitted) return '● 收到答案了，我繼續做事'
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
    out.push(
      `${mark(p.opts.length)}${p.opts.length + 1}. [${this.typeChecked[this.tab] ? 'x' : ' '}] ${this.typed[this.tab] || 'Type something'}`,
    )
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
    if (k === 'space' && this.typeRowToggles && this.cursor === opts) {
      this.typeChecked[this.tab] = !this.typeChecked[this.tab]
      return
    }
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

  /** 真機：游標停在 `Type something` 上直接打字＝那列勾起來、標題換成這段字；停在別列的字這裡當沒發生。 */
  paste(text: string) {
    this.pasted.push(text)
    if (this.tab < 2 && this.cursor === this.pages[this.tab].opts.length) {
      this.typed[this.tab] = text
      this.typeChecked[this.tab] = true
    }
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

test('預載不走進 Submit 頁：送出頁是合成的，終端從頭到尾沒離開題目（2026-09-25）', async () => {
  const tui = new FakeTui()
  const start = parseChoiceMenu(tui.screen())
  assert.ok(start)
  const seen: number[] = []
  const io = tui.io()
  const draft = await preload({ ...io, send: async (keys) => { await io.send(keys); seen.push(tui.tab) } }, start)
  assert.ok(draft)
  assert.equal(draft.pages[2].isSubmit, true)
  assert.ok(seen.every((t) => t < 2), `走進了送出頁：${seen.join(',')}`)
  assert.equal(tui.tab, 0)
})

test('起點就是 Submit 頁（被別的預載晾在那裡）時不預載，退回即時模式', async () => {
  const tui = new FakeTui()
  tui.tab = 2
  const start = parseChoiceMenu(tui.screen())
  assert.ok(start)
  assert.equal(await preload(tui.io(), start), null)
  assert.deepEqual(tui.sent, [], '一顆鍵都不送')
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

test('#908 預載中止：之後一顆導覽鍵都不再送，preload 回 null', async () => {
  const tui = new FakeTui()
  const start = parseChoiceMenu(tui.screen())
  assert.ok(start)
  const controller = new AbortController()
  const io = tui.io()
  let sends = 0
  const counted: Io = {
    ...io,
    send: async (keys) => {
      sends += 1
      await io.send(keys)
      if (sends === 2) controller.abort() // 第 2 次 moveTab 進行中，bot 離開 blocked
    },
  }
  assert.equal(await preload(counted, start, undefined, controller.signal), null)
  assert.equal(sends, 2, '中止之後不再送鍵')
  const after = sends
  await new Promise((r) => setTimeout(r, 5))
  assert.equal(sends, after)
})

test('#908 moveTab：signal 已中止就一顆鍵都不送', async () => {
  const tui = new FakeTui()
  const menu = parseChoiceMenu(tui.screen())
  assert.ok(menu)
  const controller = new AbortController()
  controller.abort()
  assert.equal(await moveTab(tui.io(), 1, menu, controller.signal), null)
  assert.deepEqual(tui.sent, [])
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

test('未送出的複選頁即使勾選沒變也要先按該頁 Submit', async () => {
  const tui = new FakeTui()
  tui.checked[0] = [true, false, false]
  const start = parseChoiceMenu(tui.screen())
  assert.ok(start)
  const draft = await preload(tui.io(), start)
  assert.ok(draft)
  // 使用者保留第一頁目前的勾選；第二頁與送出頁沒有答案。
  const want = [wantOf(draft.pages[0]), wantOf(draft.pages[1]), []]

  const res = await commit(tui.io(), draft, want)

  assert.deepEqual(res, { ok: true })
  assert.equal(tui.pageDone[0], true)
  assert.equal(tui.submitted, true)
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
    { label: '換身份門檻', q: 'cc2 撞到哪一種才換下一個身份？', opts: ['7d 或 limit_hit 才換', '任一桶撞限就換', '只看 7d'] },
    { label: 'Fable 用完', q: 'Fable 用完怎麼辦？', opts: ['切 opus', '換身份', '停住等'] },
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
    if (this.submitted) return '● 收到答案了，我繼續做事'
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
  tui.submitted = false // 交卷後畫面已離開 review，這裡要看 review 上的答案
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

test('複選頁勾 Type something 並打字：貼字 → 那列勾起來換成這段字 → 按這頁的 Submit（2026-09-25 真機）', async () => {
  const tui = new FakeTui()
  const start = parseChoiceMenu(tui.screen())
  assert.ok(start)
  const draft = await preload(tui.io(), start)
  assert.ok(draft)
  assert.equal(typedAnswerHere(draft.pages[1], draft.pages[1].choices[3]), true, '複選頁也給打字框')
  // 第 2 頁勾「站內搜尋」與「Type something」、打「另外要 RSS」。
  tui.sent = []
  const res = await commit(tui.io(), draft, [[], [true, false, false, true], []], undefined, ['', '另外要 RSS', ''])
  assert.equal(res.ok, true, res.error)
  assert.deepEqual(tui.pasted, ['另外要 RSS'])
  assert.deepEqual(tui.typed, ['', '另外要 RSS'])
  assert.deepEqual(tui.typeChecked, [false, true])
  assert.deepEqual(tui.checked[1], [true, false, false])
  assert.equal(tui.pageDone[1], true)
  assert.equal(tui.submitted, true)
  // 貼字之後那列不能再被 space／Enter 翻掉：貼完到按這頁 Submit 之間沒有落在那列上的翻轉鍵。
  const pasteAt = tui.sent.length
  assert.ok(pasteAt > 0)
})

test('複選頁勾了 Type something 卻沒打字：整批不送，一顆鍵都沒動', async () => {
  const tui = new FakeTui()
  const start = parseChoiceMenu(tui.screen())
  assert.ok(start)
  const draft = await preload(tui.io(), start)
  assert.ok(draft)
  tui.sent = []
  const res = await commit(tui.io(), draft, [[], [true, false, false, true], []], undefined, ['', '  ', ''])
  assert.equal(res.ok, false)
  assert.match(res.error ?? '', /沒有打字/)
  assert.deepEqual(tui.sent, [])
  assert.deepEqual(tui.pasted, [])
  assert.equal(tui.submitted, false)
})

test('複選頁貼字後那列沒勾起來（終端沒吃到）：停在這一題，不按 Submit', async () => {
  const tui = new FakeTui()
  const start = parseChoiceMenu(tui.screen())
  assert.ok(start)
  const draft = await preload(tui.io(), start)
  assert.ok(draft)
  const io = tui.io()
  io.paste = async (text) => {
    tui.pasted.push(text)
  }
  const res = await commit(io, draft, [[], [true, false, false, true], []], undefined, ['', '另外要 RSS', ''])
  assert.equal(res.ok, false)
  assert.match(res.error ?? '', /自訂文字/)
  assert.equal(tui.pageDone[1], false)
  assert.equal(tui.submitted, false)
})

test('複選頁的 Type something 已經在終端勾好：照舊留著、不重貼，也准取消', async () => {
  const tui = new FakeTui()
  tui.typeChecked[1] = true
  tui.typed[1] = '終端上打好的'
  tui.tab = 0
  const start = parseChoiceMenu(tui.screen())
  assert.ok(start)
  const draft = await preload(tui.io(), start)
  assert.ok(draft)
  assert.deepEqual(wantOf(draft.pages[1]), [false, false, false, true])
  const kept = await commit(tui.io(), draft, [[], [true, false, false, true], []])
  assert.equal(kept.ok, true, kept.error)
  assert.deepEqual(tui.pasted, [])
  assert.deepEqual(tui.typeChecked, [false, true])
})

test('單選頁的 Type something 照舊可以打字', () => {
  const menu = parseChoiceMenu(new FakeRadioTui().screen())
  assert.ok(menu)
  const row = menu.choices.find((c) => /type something/i.test(c.title))
  assert.ok(row)
  assert.equal(typedAnswerHere(menu, row), true)
})

/**
 * 真機單選問卷（2026-10-02 事發：3 題單選＋送出頁，第 1 題自訂文字）。與 `FakeRadioTui` 的差別是照真機的鍵語意：
 * 單選頁 Enter 選定後**自己跳下一個分頁**（最後一題跳到 Review）；`Type something.` 游標停上去直接貼字，Enter 才答完並跳頁。
 * `wrap`：真機在最後一個分頁按 → 會不會繞回第一個。
 */
class FakeAutoRadioTui {
  tab = 0
  cursor = 0
  chosen: (number | null)[] = [null, null, null]
  typed: string[] = ['', '', '']
  sent: string[] = []
  pasted: string[] = []
  submitted = false
  wrap = false
  /** Enter 之後畫面要再被讀幾次才換頁（模擬 TUI 重畫比 daemon 回得慢）。 */
  lag = 0
  private pending: { to: number; reads: number } | null = null

  private readonly pages = [
    { label: '拋單倉庫', q: 'wa_inbship_transmit_warehouse 新流程之後這段要怎麼處理？', opts: ['保留現狀 (Recommended)', '移到第三段'] },
    { label: '目標狀態', q: '「待接收」是指 NS Inbound Shipment 的哪個狀態？', opts: ['inTransit (Recommended)', 'partiallyReceived'] },
    { label: 'go API', q: 'go service 的 Slack API 放哪裡、怎麼認證？', opts: ['stock-server 新增端點 (Recommended)', '獨立服務'] },
  ]
  private readonly last = 3

  private rows(): number {
    return this.tab < this.last ? this.pages[this.tab].opts.length + 2 : 2
  }

  private tabBar(): string {
    const cells = this.pages.map((p, i) => `${this.chosen[i] !== null ? '☒' : '☐'} ${p.label}`)
    cells.push('✔ Submit')
    return `←  ${cells.join('  ')}  →`
  }

  screen(): string {
    if (this.submitted) return '● 收到答案了，我繼續做事'
    if (this.pending && this.pending.reads-- <= 0) {
      this.tab = this.pending.to
      this.cursor = 0
      this.pending = null
    }
    const mark = (i: number) => (this.cursor === i ? '❯ ' : '  ')
    const out = ['前面一堆別的輸出', '─'.repeat(60), this.tabBar(), '']
    if (this.tab === this.last) {
      out.push('Review your answers', '')
      this.pages.forEach((p, i) => {
        let a = '（沒選）'
        if (this.chosen[i] === p.opts.length) a = this.typed[i]
        else if (this.chosen[i] !== null) a = p.opts[this.chosen[i]!]
        out.push(` ● ${p.q}`, `   → ${a}`)
      })
      out.push('', 'Ready to submit your answers?', '', `${mark(0)}1. Submit answers`, `${mark(1)}2. Cancel`)
      return out.join('\n')
    }
    const p = this.pages[this.tab]
    out.push(p.q, '')
    p.opts.forEach((o, i) => out.push(`${mark(i)}${i + 1}. ${o}`))
    const t = p.opts.length
    const typed = this.typed[this.tab]
    out.push(`${mark(t)}${t + 1}. ${typed ? `${typed}${this.chosen[this.tab] === t ? ' ✔' : ''}` : 'Type something.'}`)
    out.push('─'.repeat(60))
    out.push(`${mark(t + 1)}${t + 2}. Chat about this`)
    out.push('Enter to select · Tab/Arrow keys to navigate · Esc to cancel')
    return out.join('\n')
  }

  key(k: string) {
    this.sent.push(k)
    if (k === 'right' || k === 'tab') {
      if (this.tab < this.last) this.tab += 1
      else if (this.wrap) this.tab = 0
      else return
      this.cursor = 0
      return
    }
    if (k === 'left' || k === 'shift+tab') {
      if (this.tab > 0) this.tab -= 1
      else if (this.wrap) this.tab = this.last
      else return
      this.cursor = 0
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
    if (this.tab === this.last) {
      if (k === 'enter' && this.cursor === 0) this.submitted = true
      return
    }
    if (k === 'enter') {
      this.chosen[this.tab] = this.cursor
      if (this.lag > 0) this.pending = { to: this.tab + 1, reads: this.lag }
      else {
        this.tab += 1
        this.cursor = 0
      }
    }
  }

  paste(text: string) {
    this.pasted.push(text)
    if (this.tab < this.last && this.cursor === this.pages[this.tab].opts.length) this.typed[this.tab] = text
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

for (const wrap of [false, true]) {
  for (const lag of [0, 1, 3]) {
    test(`真機單選問卷（Enter 自動跳頁；wrap=${wrap} lag=${lag}）：第 1 題自訂文字、2/3 題選推薦 → 交卷`, async () => {
      const tui = new FakeAutoRadioTui()
      tui.wrap = wrap
      tui.lag = lag
      const start = parseChoiceMenu(tui.screen())
      assert.ok(start)
      const draft = await preload(tui.io(), start)
      assert.ok(draft)
      const want = [[false, false, true, false], [true, false, false, false], [true, false, false, false], []]
      tui.sent = []
      const res = await commit(tui.io(), draft, want, undefined, ['拿掉拋單', '', '', ''])
      assert.deepEqual(res, { ok: true })
      assert.deepEqual(tui.chosen, [2, 0, 0])
      assert.equal(tui.typed[0], '拿掉拋單')
      assert.equal(tui.submitted, true, `最後沒按到 Submit answers：${tui.sent.join(',')}`)
    })
  }
}

const SURVEY_WANT = [[false, false, true, false], [true, false, false, false], [true, false, false, false], []]
const SURVEY_CUSTOM = ['拿掉拋單', '', '', '']

test('讀取終端失敗保留為錯誤；成功讀取但沒有選單才回 null', async () => {
  await assert.rejects(readChoiceMenu(async () => { throw new Error('GET terminal 503') }), /GET terminal 503/)
  assert.equal(await readChoiceMenu(async () => '一般終端輸出，沒有選單'), null)
})

test('真機單選問卷：終端已經停在 Review（上一輪最後那顆 Enter 沒送到），再按一次送出 → 不重答、只按 Submit answers', async () => {
  const tui = new FakeAutoRadioTui()
  const draft = await preload(tui.io(), parseChoiceMenu(tui.screen())!)
  assert.ok(draft)
  // 第一輪：除了最後的 Submit answers 都照真機走完，終端停在 Review。
  const lossy = tui.io()
  let lost = 0
  const first = await commit(
    { ...lossy, send: async (keys) => { if (tui.tab === 3 && keys[0] === 'enter') lost++; else await lossy.send(keys) } },
    draft, SURVEY_WANT, undefined, SURVEY_CUSTOM,
  )
  assert.equal(first.ok, false)
  assert.equal(lost, 1)
  assert.match(first.error ?? '', /還停在送出頁/)
  assert.equal(tui.tab, 3)
  assert.equal(tui.submitted, false)

  tui.sent = []
  const again = await commit(tui.io(), draft, SURVEY_WANT, undefined, SURVEY_CUSTOM)
  assert.deepEqual(again, { ok: true })
  assert.equal(tui.submitted, true)
  assert.deepEqual(tui.sent, ['enter'], '答案已經在 Review 上了，不該退回去重答')
  assert.deepEqual(tui.chosen, [2, 0, 0])
})

test('真機單選問卷：使用者在 Review 頁改了一題的答案再按送出 → 只退回去改那一題，其他不動', async () => {
  const tui = new FakeAutoRadioTui()
  const draft = await preload(tui.io(), parseChoiceMenu(tui.screen())!)
  assert.ok(draft)
  const first = await commit(tui.io(), draft, SURVEY_WANT, undefined, SURVEY_CUSTOM)
  assert.deepEqual(first, { ok: true })
  tui.submitted = false
  tui.tab = 3
  tui.cursor = 0
  const changed = SURVEY_WANT.map((r) => r.slice())
  changed[2] = [false, true, false, false]
  tui.sent = []
  const res = await commit(tui.io(), draft, changed, undefined, SURVEY_CUSTOM)
  assert.deepEqual(res, { ok: true })
  assert.deepEqual(tui.chosen, [2, 0, 1])
  assert.equal(tui.submitted, true)
  assert.equal(tui.pasted.length, 1, '第 1 題的自訂文字不重貼')
})

test('最後一顆 Enter 沒被吃掉：回一句看得懂的錯誤，不是靜靜回 ok', async () => {
  const tui = new FakeAutoRadioTui()
  const draft = await preload(tui.io(), parseChoiceMenu(tui.screen())!)
  assert.ok(draft)
  const io = tui.io()
  const res = await commit(
    { ...io, send: async (keys) => { if (!(tui.tab === 3 && keys[0] === 'enter')) await io.send(keys) } },
    draft, SURVEY_WANT, undefined, SURVEY_CUSTOM,
  )
  assert.equal(res.ok, false)
  assert.match(res.error ?? '', /Submit answers/)
})

test('交卷後 GET 503／逾時都讀不到：結果不明要回錯誤，不能當成已離開送出頁', async () => {
  const tui = new FakeAutoRadioTui()
  const draft = await preload(tui.io(), parseChoiceMenu(tui.screen())!)
  assert.ok(draft)
  const base = tui.io()
  let finalEnter = false
  let confirmationReads = 0
  const io: Io = {
    ...base,
    send: async (keys) => {
      await base.send(keys)
      if (tui.tab === 3 && keys[0] === 'enter' && tui.submitted) finalEnter = true
    },
    read: async () => {
      if (finalEnter) {
        confirmationReads += 1
        throw new Error(confirmationReads === 1 ? 'GET terminal 503' : 'GET terminal timeout')
      }
      return base.read()
    },
  }

  const result = await commit(io, draft, SURVEY_WANT, undefined, SURVEY_CUSTOM)
  assert.equal(result.ok, false)
  assert.match(result.error ?? '', /無法確認是否交卷/)
  assert.equal(confirmationReads, 6, '讀取失敗會用完剩餘確認次數，不回成功')
  assert.equal(tui.submitted, true)
})

test('交卷確認先 GET 失敗、之後仍讀到 Submit：回未交卷錯誤', async () => {
  const tui = new FakeAutoRadioTui()
  const draft = await preload(tui.io(), parseChoiceMenu(tui.screen())!)
  assert.ok(draft)
  const base = tui.io()
  let finalEnter = false
  let confirmationReads = 0
  const io: Io = {
    ...base,
    send: async (keys) => {
      if (tui.tab === 3 && keys[0] === 'enter') {
        finalEnter = true
        return
      }
      await base.send(keys)
    },
    read: async () => {
      if (finalEnter) {
        confirmationReads += 1
        if (confirmationReads === 1) throw new Error('GET terminal 503')
      }
      return base.read()
    },
  }

  const result = await commit(io, draft, SURVEY_WANT, undefined, SURVEY_CUSTOM)
  assert.equal(result.ok, false)
  assert.match(result.error ?? '', /還停在送出頁/)
  assert.ok(confirmationReads > 1)
  assert.equal(tui.submitted, false)
})

test('交卷確認先 GET 失敗、之後成功讀到離開 Submit：才回成功', async () => {
  const tui = new FakeAutoRadioTui()
  const draft = await preload(tui.io(), parseChoiceMenu(tui.screen())!)
  assert.ok(draft)
  const base = tui.io()
  let finalEnter = false
  let confirmationReads = 0
  const io: Io = {
    ...base,
    send: async (keys) => {
      await base.send(keys)
      if (tui.tab === 3 && keys[0] === 'enter' && tui.submitted) finalEnter = true
    },
    read: async () => {
      if (finalEnter && confirmationReads++ === 0) throw new Error('GET terminal timeout')
      return base.read()
    },
  }

  const result = await commit(io, draft, SURVEY_WANT, undefined, SURVEY_CUSTOM)
  assert.deepEqual(result, { ok: true })
  assert.equal(tui.submitted, true)
  assert.equal(confirmationReads, 2, '讀取錯誤後下一次有效觀測到離頁才算成功')
})

/** 翻頁偵測（2026-10-17，#1816）：兩頁題目都切掉（null）或問句相同時，只靠題目分不出換頁。 */
const PAGE_TABS: TuiTab[] = [
  { label: 'server', done: false, submit: false },
  { label: 'client', done: false, submit: false },
  { label: 'Submit', done: false, submit: true },
]

function pageMenu(tab: number, question: string | null, option: string): TuiChoiceMenu {
  return {
    question,
    context: [],
    command: [],
    commandCut: false,
    notes: [],
    choices: [
      { number: 1, title: `${option} A`, detail: '', preview: '', current: false, checked: null },
      { number: 2, title: `${option} B`, detail: '', preview: '', current: false, checked: null },
    ],
    cursor: 0,
    footer: false,
    multi: false,
    tabs: PAGE_TABS,
    tabAt: tab,
    review: [],
    submit: null,
  }
}

/** 假 TUI：`moves` 內的鍵才真的換頁（右為 +1、左為 -1），其餘鍵沒反應。 */
function pagedIo(pages: TuiChoiceMenu[], start: number, moves: string[]) {
  const sent: string[] = []
  let at = start
  const io: Io = {
    read: async () => pages[at],
    send: async (keys) => {
      sent.push(...keys)
      if (!moves.includes(keys[0])) return
      const step = keys[0] === 'right' || keys[0] === 'tab' ? 1 : -1
      at = Math.max(0, Math.min(pages.length - 1, at + step))
    },
    wait: async () => {},
    paste: async () => {},
  }
  return { io, sent }
}

const reviewPage = (): TuiChoiceMenu => ({ ...pageMenu(2, null, 'review'), review: [{ question: 'server', answer: 'A' }] })

test('#1816 題目切掉（兩頁都是 null）：按 right 已換頁就停在第 2 頁，不補 tab 跳過第 2 題', async () => {
  const pages = [pageMenu(0, null, 'server'), pageMenu(1, null, 'client'), reviewPage()]
  const { io, sent } = pagedIo(pages, 0, ['right', 'tab'])
  const got = await moveTab(io, 1, pages[0])
  assert.equal(got?.tabAt, 1)
  assert.deepEqual(sent, ['right'])
})

test('#1816 兩題問句相同、選項不同：按 right 已換頁就停在第 2 頁，不補 tab', async () => {
  const pages = [pageMenu(0, 'Pick one?', 'server'), pageMenu(1, 'Pick one?', 'client'), reviewPage()]
  const { io, sent } = pagedIo(pages, 0, ['right', 'tab'])
  const got = await moveTab(io, 1, pages[0])
  assert.equal(got?.tabAt, 1)
  assert.deepEqual(sent, ['right'])
})

test('#1816 真的沒換頁（有問句）：right 沒反應才照舊補 tab，tab 換頁就回傳', async () => {
  const pages = [pageMenu(0, 'Pick one?', 'server'), pageMenu(1, 'Pick one?', 'client'), reviewPage()]
  const { io, sent } = pagedIo(pages, 0, ['tab'])
  const got = await moveTab(io, 1, pages[0])
  assert.equal(got?.tabAt, 1)
  assert.deepEqual(sent, ['right', 'tab'])
})

test('#1816 題目切掉又沒換頁：分不出是翻了還是沒翻，不補 tab，回 null', async () => {
  const pages = [pageMenu(0, null, 'server'), pageMenu(1, null, 'client'), reviewPage()]
  const { io, sent } = pagedIo(pages, 0, [])
  assert.equal(await moveTab(io, 1, pages[0]), null)
  assert.deepEqual(sent, ['right'])
})

test('#1816 locate：同問句、同選項數、選項不同，靠選項認出是哪一頁', () => {
  const draft: Draft = {
    pages: [0, 1].map((tab) => {
      const m = pageMenu(tab, 'Pick one?', tab === 0 ? 'server' : 'client')
      const page: DraftPage = { tab, label: PAGE_TABS[tab].label, question: m.question, choices: m.choices, multi: false, hasSubmitRow: false, review: [], isSubmit: false }
      return page
    }),
    startTab: 0,
    tabs: PAGE_TABS,
  }
  assert.equal(locate(draft, pageMenu(1, 'Pick one?', 'client')), 1)
  assert.equal(locate(draft, pageMenu(0, 'Pick one?', 'server')), 0)
})

test('#1816 locate：兩頁完全一樣分不出來時回 null，不猜第一頁', () => {
  const pages = [0, 1].map((tab) => {
    const m = pageMenu(tab, 'Pick one?', 'same')
    const page: DraftPage = { tab, label: PAGE_TABS[tab].label, question: m.question, choices: m.choices, multi: false, hasSubmitRow: false, review: [], isSubmit: false }
    return page
  })
  const draft: Draft = { pages, startTab: 0, tabs: PAGE_TABS }
  assert.equal(locate(draft, pageMenu(1, 'Pick one?', 'same')), null)
})

test('#1816 review：導覽途中 locate 認不出在哪一頁 → 不再送導覽鍵，回錯誤', async () => {
  // 起點（第 1 頁）有獨特的選項，導覽後的畫面同時像第 2、3 頁，分不出在哪一頁。
  const tabs: TuiTab[] = [...PAGE_TABS.slice(0, 2), { label: 'third', done: false, submit: false }, PAGE_TABS[2]]
  const menu = (tab: number, option: string): TuiChoiceMenu => ({ ...pageMenu(tab, 'Pick one?', option), tabs })
  const pageOf = (tab: number, option: string): DraftPage => {
    const m = menu(tab, option)
    return { tab, label: tabs[tab].label, question: m.question, choices: m.choices, multi: false, hasSubmitRow: false, review: [], isSubmit: false }
  }
  const draft: Draft = { pages: [pageOf(0, 'start'), pageOf(1, 'same'), pageOf(2, 'same')], startTab: 0, tabs }
  const screens = [menu(0, 'start'), menu(1, 'same')]
  const sent: string[] = []
  let at = 0
  const io: Io = {
    read: async () => screens[at],
    send: async (keys) => {
      sent.push(...keys)
      if (keys[0] === 'right') at = 1
    },
    wait: async () => {},
    paste: async () => {},
  }
  const res = await commit(io, draft, [[], [true, false], []])
  assert.equal(res.ok, false)
  assert.match(res.error ?? '', /分不出/)
  assert.deepEqual(sent, ['right'])
})

test('#1816 review：問句為 null 時，只靠選項數對到一頁的 locate 不認', () => {
  const page = (tab: number, count: number): DraftPage => ({
    tab,
    label: PAGE_TABS[tab].label,
    question: null,
    choices: Array.from({ length: count }, (_, j) => ({ number: j + 1, title: `opt${tab}-${j}`, detail: '', preview: '', current: false, checked: null })),
    multi: false,
    hasSubmitRow: false,
    review: [],
    isSubmit: false,
  })
  const draft: Draft = { pages: [page(0, 2), page(1, 3)], startTab: 0, tabs: PAGE_TABS }
  const now = pageMenu(0, null, 'other')
  assert.equal(now.choices.length, 2)
  assert.equal(locate(draft, now), null)
})

test('#1816 起點分不出頁（兩頁完全相同）：一顆鍵都不送，直接回錯誤', async () => {
  const tabs: TuiTab[] = [...PAGE_TABS]
  const screen = pageMenu(0, 'Pick one?', 'same')
  const pageOfTwin = (tab: number): DraftPage => {
    const m = pageMenu(tab, 'Pick one?', 'same')
    return { tab, label: PAGE_TABS[tab].label, question: m.question, choices: m.choices, multi: false, hasSubmitRow: false, review: [], isSubmit: false }
  }
  const draft: Draft = { pages: [pageOfTwin(0), pageOfTwin(1)], startTab: 0, tabs }
  const sent: string[] = []
  const io: Io = {
    read: async () => screen,
    send: async (keys) => {
      sent.push(...keys)
    },
    wait: async () => {},
    paste: async () => {},
  }
  const res = await commit(io, draft, [[true, false], [true, false]])
  assert.equal(res.ok, false)
  assert.match(res.error ?? '', /分不出終端停在哪一頁，什麼都沒送出/)
  assert.deepEqual(sent, [])
})

test('#1816 起點停在 Review：起點照舊用 startTab，不走新的起點錯誤', async () => {
  const tabs: TuiTab[] = [...PAGE_TABS]
  const pageOfTwin = (tab: number): DraftPage => {
    const m = pageMenu(tab, 'Pick one?', 'same')
    return { tab, label: PAGE_TABS[tab].label, question: m.question, choices: m.choices, multi: false, hasSubmitRow: false, review: [], isSubmit: false }
  }
  const draft: Draft = { pages: [pageOfTwin(0), pageOfTwin(1)], startTab: 0, tabs }
  // 終端停在 Review：分不出頁，但 isSubmitMenu 為真，照舊從 startTab 走向送出頁；畫面不動就在導覽時停下。
  const review: TuiChoiceMenu = {
    ...pageMenu(2, 'Review?', 'review'),
    review: [{ question: 'server', answer: 'A' }],
    choices: [
      { number: 1, title: 'Submit answers', detail: '', preview: '', current: true, checked: null },
      { number: 2, title: 'Cancel', detail: '', preview: '', current: false, checked: null },
    ],
  }
  const sent: string[] = []
  const io: Io = {
    read: async () => review,
    send: async (keys) => {
      sent.push(...keys)
    },
    wait: async () => {},
    paste: async () => {},
  }
  const res = await commit(io, draft, [[], []])
  assert.equal(res.ok, false)
  assert.match(res.error ?? '', /走不到送出頁/)
  assert.deepEqual(sent, ['right', 'tab'])
})

test('#1818 commit：多行自訂答案貼上後只出現第一行（沒有裁切記號）→ 不按 Enter、不交卷', async () => {
  const tabs: TuiTab[] = [
    { label: 'q', done: false, submit: false },
    { label: 'Submit', done: false, submit: true },
  ]
  const choice = (title: string, number: number, current: boolean) => ({ number, title, detail: '', current, checked: null, preview: '' })
  const screen = (second: string, cursor: number): TuiChoiceMenu => ({
    question: 'Q',
    context: [],
    command: [],
    commandCut: false,
    notes: [],
    choices: [choice('Option', 1, cursor === 0), choice(second, 2, cursor === 1)],
    cursor,
    footer: false,
    multi: false,
    tabs,
    tabAt: 0,
    review: [],
    submit: null,
  })
  const draft: Draft = {
    pages: [
      { tab: 0, label: 'q', question: 'Q', choices: screen('Type something.', 1).choices, multi: false, hasSubmitRow: false, review: [], isSubmit: false },
      { tab: 1, label: 'Submit', question: null, choices: [], multi: false, hasSubmitRow: false, review: [], isSubmit: true },
    ],
    startTab: 0,
    tabs,
  }
  let typed: string | null = null
  const sent: string[] = []
  const io: Io = {
    read: async () => screen(typed ?? 'Type something.', 1),
    send: async (keys) => {
      sent.push(...keys)
    },
    wait: async () => {},
    paste: async (text) => {
      // 終端只留下第一行。
      typed = text.split('\n')[0]
    },
  }
  const res = await commit(io, draft, [[false, true]], undefined, ['第一行\n第二行很重要'])
  assert.equal(res.ok, false)
  assert.match(res.error ?? '', /沒有出現/)
  assert.equal(sent.includes('enter'), false)
})
