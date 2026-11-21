export const ISSUE_PAGE_LIMIT = 50

export function openCountFromPage(prev: number | null, pageLength: number, limit = ISSUE_PAGE_LIMIT): number | null {
  return pageLength >= limit ? prev : pageLength
}
