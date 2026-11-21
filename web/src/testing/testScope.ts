import { AsyncLocalStorage } from 'node:async_hooks'

export interface TestScope {
  readonly name: string
  ended: boolean
}

const scopes = new AsyncLocalStorage<TestScope>()
let current: TestScope | null = null

export function runTestBody<T>(name: string, body: () => T): T {
  const scope: TestScope = { name, ended: false }
  current = scope
  return scopes.run(scope, body)
}

export function endCurrentTest(): void {
  if (current) current.ended = true
}

export function endedTestScope(): TestScope | null {
  const scope = scopes.getStore()
  return scope?.ended ? scope : null
}

export function currentTestScope(): TestScope | undefined {
  return scopes.getStore()
}
