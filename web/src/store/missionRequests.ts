/** Keep an uncertain send's ID across retries; coalesce concurrent sends of that draft. */
export function missionRequests(newId: () => string) {
  const ids = new Map<string, string>()
  const active = new Map<string, Promise<unknown>>()
  return function send<T>(op: string, missionId: string, text: string, request: (id: string) => Promise<T>): Promise<T> {
    const key = JSON.stringify([op, missionId, text.trim()])
    const running = active.get(key)
    if (running) return running as Promise<T>
    const id = ids.get(key) ?? newId()
    ids.set(key, id)
    const pending = Promise.resolve().then(() => request(id)).then((result) => {
      // A later intentional send, even with identical text, is a new request.
      ids.delete(key)
      return result
    }).finally(() => active.delete(key))
    active.set(key, pending)
    return pending
  }
}
