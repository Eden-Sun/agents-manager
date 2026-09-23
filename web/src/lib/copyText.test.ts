import test from 'node:test'
import assert from 'node:assert/strict'
import { copyText } from './copyText.ts'

type ThrowAt = 'focus' | 'select' | 'setSelectionRange' | 'execCommand'

function replaceGlobal(name: string, value: unknown): () => void {
  const descriptor = Object.getOwnPropertyDescriptor(globalThis, name)
  Object.defineProperty(globalThis, name, { configurable: true, enumerable: descriptor?.enumerable ?? true, writable: true, value })
  return () => {
    if (descriptor) Object.defineProperty(globalThis, name, descriptor)
    else Reflect.deleteProperty(globalThis, name)
  }
}

function installDom(options: { throwAt?: ThrowAt; execResult?: boolean; writeText?: (text: string) => Promise<void> | void } = {}) {
  const children: any[] = []
  const attributes = new Map<string, string>()
  let activeElement: any
  const previous = {
    focusCalls: 0,
    focus() {
      this.focusCalls += 1
      activeElement = previous
    },
  }
  activeElement = previous

  const body: any = {
    appendChild(element: any) {
      children.push(element)
      element.parentNode = body
      return element
    },
    removeChild(element: any) {
      const index = children.indexOf(element)
      if (index < 0) throw new Error('element is not a child')
      children.splice(index, 1)
      element.parentNode = null
      return element
    },
  }
  const textarea: any = {
    value: '',
    style: {} as Record<string, string>,
    parentNode: null,
    focusOptions: null as unknown,
    range: null as [number, number] | null,
    setAttribute(name: string, value: string) {
      attributes.set(name, value)
    },
    focus(focusOptions: unknown) {
      this.focusOptions = focusOptions
      activeElement = textarea
      if (options.throwAt === 'focus') throw new Error('focus failed')
    },
    select() {
      if (options.throwAt === 'select') throw new Error('select failed')
    },
    setSelectionRange(start: number, end: number) {
      this.range = [start, end]
      if (options.throwAt === 'setSelectionRange') throw new Error('setSelectionRange failed')
    },
  }
  let execCommandCalls = 0
  const documentMock = {
    body,
    createElement(tag: string) {
      assert.equal(tag, 'textarea')
      return textarea
    },
    get activeElement() {
      return activeElement
    },
    execCommand(command: string) {
      execCommandCalls += 1
      assert.equal(command, 'copy')
      if (options.throwAt === 'execCommand') throw new Error('execCommand failed')
      return options.execResult ?? false
    },
  }
  let selectionClears = 0
  const windowMock = {
    getSelection() {
      return {
        removeAllRanges() {
          selectionClears += 1
        },
      }
    },
  }
  let writeTextCalls = 0
  const navigatorMock = {
    clipboard: options.writeText
      ? {
          writeText(text: string) {
            writeTextCalls += 1
            return options.writeText?.(text)
          },
        }
      : undefined,
  }

  const restoreDocument = replaceGlobal('document', documentMock)
  const restoreWindow = replaceGlobal('window', windowMock)
  const restoreNavigator = replaceGlobal('navigator', navigatorMock)

  return {
    attributes,
    children,
    get activeElement() {
      return activeElement
    },
    execCommandCalls: () => execCommandCalls,
    previous,
    selectionClears: () => selectionClears,
    textarea,
    writeTextCalls: () => writeTextCalls,
    restore() {
      restoreNavigator()
      restoreWindow()
      restoreDocument()
    },
  }
}

test('legacy copy 成功時選取文字、移除暫存 textarea 並還原焦點', async () => {
  const dom = installDom({ execResult: true })
  try {
    assert.equal(await copyText('copy this'), true)
    assert.equal(dom.textarea.value, 'copy this')
    assert.deepEqual(dom.textarea.focusOptions, { preventScroll: true })
    assert.equal(dom.attributes.get('inputmode'), 'none')
    assert.deepEqual(dom.textarea.range, [0, 'copy this'.length])
    assert.equal(dom.execCommandCalls(), 1)
    assert.equal(dom.children.length, 0)
    assert.equal(dom.selectionClears(), 1)
    assert.equal(dom.activeElement, dom.previous)
    assert.equal(dom.previous.focusCalls, 1)
  } finally {
    dom.restore()
  }
})

test('clipboard.writeText 同步丟錯時仍回傳 resolved false', async () => {
  const dom = installDom({ writeText: () => { throw new Error('clipboard unavailable') } })
  try {
    let result!: Promise<boolean>
    assert.doesNotThrow(() => {
      result = copyText('copy this')
    })
    assert.equal(await result, false)
    assert.equal(dom.writeTextCalls(), 1)
    assert.equal(dom.children.length, 0)
    assert.equal(dom.activeElement, dom.previous)
  } finally {
    dom.restore()
  }
})

test('focus、select 或 setSelectionRange 同步失敗時清掉 textarea 並還原焦點', async () => {
  const failures: string[] = []
  for (const throwAt of ['focus', 'select', 'setSelectionRange'] as const) {
    const dom = installDom({ throwAt })
    try {
      let result: Promise<boolean> | null = null
      try {
        result = copyText('copy this')
      } catch {
        failures.push(`${throwAt}: copyText threw synchronously`)
      }
      if (result && (await result) !== false) failures.push(`${throwAt}: copyText did not resolve false`)
      if (dom.children.length !== 0) failures.push(`${throwAt}: textarea was left in the DOM`)
      if (dom.selectionClears() !== 1) failures.push(`${throwAt}: selection was not cleared`)
      if (dom.activeElement !== dom.previous) failures.push(`${throwAt}: previous focus was not restored`)
      if (dom.previous.focusCalls !== 1) failures.push(`${throwAt}: previous element was not focused`)
    } finally {
      dom.restore()
    }
  }
  assert.deepEqual(failures, [])
})

test('execCommand 丟錯時也安全回 false 並完成清理', async () => {
  const dom = installDom({ throwAt: 'execCommand' })
  try {
    assert.equal(await copyText('copy this'), false)
    assert.equal(dom.children.length, 0)
    assert.equal(dom.selectionClears(), 1)
    assert.equal(dom.activeElement, dom.previous)
  } finally {
    dom.restore()
  }
})
