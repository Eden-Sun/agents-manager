export function createLatestOnly() {
  let seq = 0
  return {
    begin: () => ++seq,
    isCurrent: (ticket: number) => ticket === seq,
  }
}
