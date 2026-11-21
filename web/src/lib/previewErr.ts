export function shownPreviewErr<T>(start: T | null, load: T | null): T | null {
  return start ?? load
}
