import { afterAll, afterEach, beforeAll, beforeEach, describe, it, mock, test as bunTest } from 'bun:test'
import { endCurrentTest, runTestBody } from '../src/testing/testScope'

;(globalThis as { __AM_SYSTEM_LANG__?: string }).__AM_SYSTEM_LANG__ = 'zh-TW'

const wrapBody = (args: unknown[]): unknown[] =>
  args.map((a) => (typeof a === 'function' && a.length === 0 ? () => runTestBody(String(args[0]), a as () => unknown) : a))
afterEach(endCurrentTest)

const test = Object.assign((...args: Parameters<typeof bunTest>) => bunTest(...(wrapBody(args) as Parameters<typeof bunTest>)), {
  describe,
  it,
  before: beforeAll,
  after: afterAll,
  beforeEach,
  afterEach,
})

for (const key of ['skip', 'todo', 'only'] as const) {
  Object.defineProperty(test, key, { get: () => bunTest[key], enumerable: true })
}

mock.module('node:test', () => ({
  default: test,
  test,
  it,
  describe,
  before: beforeAll,
  after: afterAll,
  beforeEach,
  afterEach,
}))
