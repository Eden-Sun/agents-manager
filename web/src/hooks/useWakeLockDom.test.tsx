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

it('WebKit 無手勢被拒：第一次 request reject → 觸控後成功；release 後再觸控重拿；關掉開關移除監聽並 release', async () => {
  let allowRequest = false
  let requestErrors = 0
  Object.defineProperty(navigator, 'wakeLock', {
    configurable: true,
    value: {
      request: async () => {
        requests += 1
        if (!allowRequest) {
          requestErrors += 1
          throw new DOMException('user gesture required', 'NotAllowedError')
        }
        return sentinel()
      },
    },
  })

  // 1. 偏好已開啟，第一次掛載時因為沒有手勢被 reject
  setKeepAwake(true)
  await mount(
    <>
      <Holder />
      <Status />
      <KeepAwakeToggle />
    </>,
  )
  await settle()

  assert.equal(requests, 1, '嘗試要過一次鎖')
  assert.equal(requestErrors, 1, '第一次無手勢被拒絕')
  assert.equal(active(), 'false', '未握住鎖')
  const statusEl = document.querySelector('.keep-awake-status')
  assert.ok(statusEl, '開關旁應顯示未生效提示')
  assert.match(statusEl?.textContent ?? '', /已開啟但目前未生效：點一下畫面恢復/)

  // 2. 模擬觸控（touchend），手勢啟動允許成功拿鎖
  allowRequest = true
  await act(async () => {
    window.dispatchEvent(new Event('touchend'))
  })
  await settle()

  assert.equal(requests, 2, '手勢觸發第二次 request')
  assert.equal(active(), 'true', '拿到鎖，狀態變為 active')
  assert.equal(document.querySelector('.keep-awake-status'), null, '生效後提示消失')

  // 拿到鎖後，手勢監聽應已被移除：再點擊不應再送 request
  await act(async () => {
    window.dispatchEvent(new Event('pointerdown'))
  })
  await settle()
  assert.equal(requests, 2, '拿鎖後手勢監聽已移除，不重複 request')

  // 3. 系統收回（release 事件）
  const [currentLock] = locks
  assert.ok(currentLock, '有當前鎖')
  await act(async () => {
    currentLock.released = true
    currentLock.fireRelease()
  })
  await settle()

  assert.equal(active(), 'false', '系統收回後狀態變為 false')
  const statusAfterRelease = document.querySelector('.keep-awake-status')
  assert.ok(statusAfterRelease, '收回後重新顯示未生效提示')
  assert.match(statusAfterRelease?.textContent ?? '', /已開啟但目前未生效：點一下畫面恢復/)

  // 4. release 後再觸控，會重新要鎖並成功
  await act(async () => {
    window.dispatchEvent(new Event('pointerdown'))
  })
  await settle()

  assert.equal(requests, 3, 'release 後觸控重新要鎖')
  assert.equal(active(), 'true', '重新拿到鎖')
  assert.equal(document.querySelector('.keep-awake-status'), null)

  // 5. 關掉開關：會 release 鎖並移除監聽
  const [, secondLock] = locks
  assert.ok(secondLock, '有第二顆鎖')
  assert.equal(secondLock.released, false)

  await act(async () => {
    setKeepAwake(false)
  })
  await settle()

  assert.equal(secondLock.released, true, '關閉開關後 release 鎖')
  assert.equal(active(), 'false')

  // 關閉後再觸控，監聽器已移除，不會再要鎖
  await act(async () => {
    window.dispatchEvent(new Event('touchend'))
    window.dispatchEvent(new Event('keydown'))
  })
  await settle()
  assert.equal(requests, 3, '關閉後手勢監聽已被移除，不再發送 request')
})

it('等待手勢期間關掉開關：移除手勢監聽，後續觸摸不發起 request', async () => {
  Object.defineProperty(navigator, 'wakeLock', {
    configurable: true,
    value: {
      request: async () => {
        requests += 1
        throw new DOMException('user gesture required', 'NotAllowedError')
      },
    },
  })

  setKeepAwake(true)
  await mount(
    <>
      <Holder />
      <Status />
    </>,
  )
  await settle()

  assert.equal(requests, 1, '初次嘗試被 reject')
  assert.equal(active(), 'false')

  // 在等待手勢期間直接關閉開關
  await act(async () => {
    setKeepAwake(false)
  })
  await settle()

  // 關閉後再觸發手勢事件
  await act(async () => {
    window.dispatchEvent(new Event('pointerdown'))
    window.dispatchEvent(new Event('touchend'))
    window.dispatchEvent(new Event('keydown'))
  })
  await settle()

  assert.equal(requests, 1, '關閉後手勢監聽已被卸載，不再送 request')
})
