import assert from 'node:assert/strict'
import test from 'node:test'
import { syncKidsScroll, wheelKidsScroll } from './kidsScroll.ts'

const pendingScrollEvents: FakeRow[] = []

class FakeKids {
  rows: FakeRow[] = []

  querySelectorAll<E extends Element = Element>(_selector: string): NodeListOf<E> {
    return this.rows as unknown as NodeListOf<E>
  }
}

/** Models scrollLeft clamping and the scroll event queued after a programmatic/user scroll. */
class FakeRow {
  private left = 0
  readonly scrollWidth: number
  readonly clientWidth: number
  private readonly kids: FakeKids

  constructor(scrollWidth: number, clientWidth: number, kids: FakeKids) {
    this.scrollWidth = scrollWidth
    this.clientWidth = clientWidth
    this.kids = kids
  }

  get scrollLeft() {
    return this.left
  }

  set scrollLeft(value: number) {
    const clamped = Math.min(Math.max(value, 0), this.scrollWidth - this.clientWidth)
    if (clamped === this.left) return
    this.left = clamped
    pendingScrollEvents.push(this)
  }

  closest(selector: string) {
    return selector === '.bot-kids' ? this.kids : null
  }
}

function row(kids: FakeKids, maxScroll: number) {
  const result = new FakeRow(100 + maxScroll, 100, kids)
  kids.rows.push(result)
  return result
}

function flushScrollEvents() {
  while (pendingScrollEvents.length > 0) {
    const currentTarget = pendingScrollEvents.shift()!
    syncKidsScroll({ currentTarget: currentTarget as unknown as HTMLElement })
  }
}

test('wheel scrolling reaches the longest row even when shorter rows clamp', () => {
  const kids = new FakeKids()
  const short = row(kids, 50)
  const long = row(kids, 300)
  let prevented = false

  wheelKidsScroll({
    currentTarget: kids as unknown as HTMLElement,
    deltaX: 120,
    deltaY: 0,
    shiftKey: false,
    preventDefault: () => {
      prevented = true
    },
  })
  flushScrollEvents()

  assert.equal(prevented, true)
  assert.equal(short.scrollLeft, 50)
  assert.equal(long.scrollLeft, 120, 'a short row’s clamped scroll event must not pull the long row back')
})

test('manual horizontal scrolling from either row still synchronizes the group', () => {
  const kids = new FakeKids()
  const short = row(kids, 50)
  const long = row(kids, 300)

  short.scrollLeft = 35
  flushScrollEvents()
  assert.equal(long.scrollLeft, 35)

  long.scrollLeft = 150
  flushScrollEvents()
  assert.equal(short.scrollLeft, 50, 'the short row stops at its own maximum')
  assert.equal(long.scrollLeft, 150, 'the short row’s clamp must not reverse the manual long-row scroll')

  short.scrollLeft = 20
  flushScrollEvents()
  assert.equal(long.scrollLeft, 20)
})
