export function createResyncRunner(run: () => Promise<void>): () => void {
  let running = false
  let again = false
  const loop = async () => {
    running = true
    try {
      do {
        again = false
        try {
          await run()
        } catch {
        }
      } while (again)
    } finally {
      running = false
    }
  }
  return () => {
    if (running) {
      again = true
      return
    }
    void loop()
  }
}
