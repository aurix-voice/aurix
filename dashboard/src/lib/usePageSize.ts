import { useCallback, useState } from "react";

export const PAGE_SIZES = [10, 25, 50, 100] as const;
export type PageSize = (typeof PAGE_SIZES)[number];
export const DEFAULT_PAGE_SIZE: PageSize = 25;

const STORAGE_PREFIX = "aurix.dashboard.pageSize:";

export function isPageSize(n: number): n is PageSize {
  return (PAGE_SIZES as readonly number[]).includes(n);
}

/** Stored preference → valid page size (unknown or tampered values fall back to the default). */
export function parsePageSize(raw: string | null | undefined): PageSize {
  const n = Number(raw);
  return raw != null && raw !== "" && isPageSize(n) ? n : DEFAULT_PAGE_SIZE;
}

function read(table: string): PageSize {
  try {
    return parsePageSize(localStorage.getItem(STORAGE_PREFIX + table));
  } catch {
    return DEFAULT_PAGE_SIZE;
  }
}

/** Rows-per-page preference of one table, remembered per browser. */
export function usePageSize(table: string): [PageSize, (n: number) => void] {
  const [size, setSize] = useState<PageSize>(() => read(table));
  const update = useCallback(
    (n: number) => {
      if (!isPageSize(n)) return;
      setSize(n);
      try {
        localStorage.setItem(STORAGE_PREFIX + table, String(n));
      } catch {
        /* private mode / quota: keep in-memory only */
      }
    },
    [table],
  );
  return [size, update];
}
