export function renderProbe(name: string): void {
  const probe = (globalThis as { __amRenderProbe?: (n: string) => void }).__amRenderProbe
  if (probe) probe(name)
}
