/**
 * #1199：「螢幕保持亮著」要在環境設定視窗關掉之後還握著鎖。以前鎖掛在開關元件上，視窗一關元件卸載、effect cleanup 就 release。
 * 現在握鎖的是 App 根部（`useWakeLockHolder`），開關只讀寫偏好。真的掛進 happy-dom；`navigator.wakeLock` 用假的，
 * 記下 request／release 次數。
 */
import test, { after, afterEach, before, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { useEffect, useState } from 'react'
import { act, mount, setupDom, settle, teardownDom, unmountAll } from '../testing/domHarness'
import { KeepAwakeToggle } from '../components/KeepAwakeToggle'
import { setKeepAwake, useWakeLock, useWakeLockHolder } from './useWakeLock'

interface Sentinel {
  released: boolean
  release: () => Promise<void>
  addEventListener: (type: 'release', cb: () => void) => void
  /** 觸發 release 事件（系統收回，或 release() 之後事件才晚到）。 */
  fireRelease: () => void
}

let requests = 0
let released = 0
let locks: Sentinel[] = []
const sentinel = () => {
  const cbs: Array<() => void> = []
  const s: Sentinel = {
    released: false,
    release: async () => {
      s.released = true
      released += 1
    },
    addEventListener: (_type, cb) => {
      cbs.push(cb)
    },
    fireRelease: () => {
      for (const cb of cbs) cb()
    },
  }
  locks.push(s)
  return s
}

/** 把全域 holding 狀態讀出來：DOM 上寫 active。 */
function Status() {
  const { active } = useWakeLock()
  return <output data-active={String(active)} />
}
const active = () => document.querySelector('output')?.dataset.active

/** App 根部的替身：一直掛著，負責握鎖。 */
function Holder() {
  useWakeLockHolder()
  return null
}

let setShow: (v: boolean) => void = () => {}
/** 環境設定視窗的替身：開著才掛開關元件。 */
function SettingsWindow() {
  const [show, set] = useState(true)
  useEffect(() => {
    setShow = set
  }, [set])
  return show ? <KeepAwakeToggle /> : null
}

const it = (name: string, fn: () => Promise<void>) => test(name, { timeout: 30_000 }, fn)

before(setupDom)
beforeEach(() => {
  requests = 0
  released = 0
  locks = []
  Object.defineProperty(navigator, 'wakeLock', {
    configurable: true,
    value: {
      request: async () => {
        requests += 1
        return sentinel()
      },
    },
  })
})
afterEach(async () => {
  await unmountAll()
  delete (navigator as { wakeLock?: unknown }).wakeLock
  setKeepAwake(false)
})
after(async () => {
  await teardownDom()
})

it('開著、關掉環境設定視窗（開關元件卸載）之後鎖還握著，不放掉', async () => {
  setKeepAwake(true)
  await mount(
    <>
      <Holder />
      <SettingsWindow />
    </>,
  )
  await settle()
  assert.equal(requests, 1, '開著就拿一顆鎖')
  await act(async () => setShow(false))
  await settle()
  assert.equal(released, 0, '視窗關了，鎖不該被放掉')
})

it('關掉開關才釋放', async () => {
  setKeepAwake(true)
  await mount(
    <>
      <Holder />
      <SettingsWindow />
    </>,
  )
  await settle()
  assert.equal(requests, 1)
  await act(async () => setKeepAwake(false))
  await settle()
  assert.equal(released, 1, '偏好關了，握著的那顆要還')
})

it('偏好已經是開的：不必掛開關元件就會拿鎖', async () => {
  setKeepAwake(true)
  await mount(<Holder />)
  await settle()
  assert.equal(requests, 1)
})

it('開關顯示現在有沒有握住：拿到鎖之後寫「開著」', async () => {
  setKeepAwake(true)
  await mount(<Holder />)
  await settle()
  await mount(<KeepAwakeToggle />)
  await settle()
  const note = document.querySelector('.keep-awake-note')?.textContent ?? ''
  assert.ok(note.startsWith('開著：'), `實際寫：${note}`)
})

it('快速關→開：舊鎖的 release 事件晚到，不可把新鎖的「握著」清成沒有', async () => {
  setKeepAwake(true)
  await mount(
    <>
      <Holder />
      <Status />
    </>,
  )
  await settle()
  assert.equal(requests, 1)
  await act(async () => setKeepAwake(false))
  await settle()
  assert.equal(released, 1, '關掉，舊鎖還')
  await act(async () => setKeepAwake(true))
  await settle()
  assert.equal(requests, 2, '重新開，拿到新鎖')
  assert.equal(active(), 'true')
  // 舊鎖的 release 事件這時才到。
  const [oldLock, newLock] = locks
  await act(async () => {
    oldLock.released = true
    oldLock.fireRelease()
  })
  await settle()
  assert.equal(active(), 'true', '新鎖還握著，不能被舊鎖的事件清掉')
  assert.equal(newLock.released, false, '新鎖不該被釋放')
})

it('正常關閉：系統收回目前握著的鎖，狀態變回沒握住', async () => {
  setKeepAwake(true)
  await mount(
    <>
      <Holder />
      <Status />
    </>,
  )
  await settle()
  assert.equal(active(), 'true')
  const [lock] = locks
  await act(async () => {
    lock.released = true
    lock.fireRelease()
  })
  await settle()
  assert.equal(active(), 'false', '目前這顆被收回，要寫沒握住')
})
