/**
 * 【暫時版：由 agents-manager-4ywp1c-sw-sk 的分支（4ywp1c/xterm-input）提供正式版；這個檔不進 4ywp1c/xterm-web 的 commit。】
 * 簽名照約定：xterm onData 字串 → 依序的 herdr 操作；createInputBatcher 把同一 frame 內的 onData 合成一次 send。
 */

export type ShellOp = { kind: 'text'; text: string } | { kind: 'keys'; keys: string[] }

const CONTROL_KEYS: Record<string, string> = {
  '\r': 'enter',
  '\n': 'enter',
  '\x7f': 'backspace',
  '\b': 'backspace',
  '\x03': 'ctrl+c',
  '\t': 'tab',
  '\x1b': 'esc',
  '\x1b[A': 'up',
  '\x1b[B': 'down',
  '\x1b[C': 'right',
  '\x1b[D': 'left',
}

export function dataToOps(data: string): ShellOp[] {
  const ops: ShellOp[] = []
  const push = (op: ShellOp) => {
    const last = ops[ops.length - 1]
    if (op.kind === 'text' && last?.kind === 'text') last.text += op.text
    else if (op.kind === 'keys' && last?.kind === 'keys') last.keys.push(...op.keys)
    else ops.push(op)
  }
  const key = CONTROL_KEYS[data]
  if (key) push({ kind: 'keys', keys: [key] })
  else if (data) push({ kind: 'text', text: data })
  return ops
}

export function createInputBatcher(
  send: (ops: ShellOp[]) => void,
  schedule: (cb: () => void) => void = (cb) => requestAnimationFrame(() => cb()),
): (data: string) => void {
  let pending: string | null = null
  return (data: string) => {
    if (pending === null) {
      pending = ''
      schedule(() => {
        const batch = pending ?? ''
        pending = null
        send(dataToOps(batch))
      })
    }
    pending += data
  }
}
