import { ChevronLeft, ChevronRight, ChevronsUpDown, ChevronUp, ChevronDown } from "lucide-react";
import { isValidElement, type ReactNode } from "react";

import { useT } from "@/i18n";
import { cn } from "@/lib/cn";
import { PAGE_SIZES } from "@/lib/usePageSize";

import { Button } from "./Button";
import { NativeSelect } from "./Input";
import { QueryError } from "./Page";
import { EmptyState, Skeleton } from "./Primitives";

export interface Column<Row> {
  key: string;
  header: ReactNode;
  cell: (row: Row) => ReactNode;
  className?: string;
  headerClassName?: string;
  align?: "left" | "right" | "center";
  /** Sorting accessor; when set the header is clickable. */
  sort?: (row: Row) => number | string | null | undefined;
  hidden?: boolean;
  width?: string;
}

export interface SortState {
  key: string;
  dir: "asc" | "desc";
}

export interface DataTableProps<Row> {
  rows: Row[] | undefined;
  columns: Column<Row>[];
  rowKey: (row: Row) => string;
  loading?: boolean;
  /** Raw query error (rendered via `QueryError`) or a ready element. */
  error?: unknown;
  empty?: ReactNode;
  onRowClick?: (row: Row) => void;
  rowClassName?: (row: Row) => string | undefined;
  sort?: SortState | null;
  onSortChange?: (s: SortState | null) => void;
  dense?: boolean;
  className?: string;
  selectedKey?: string | null;
  footer?: ReactNode;
  skeletonRows?: number;
}

export function sortRows<Row>(rows: Row[], columns: Column<Row>[], sort: SortState | null | undefined): Row[] {
  if (!sort) return rows;
  const col = columns.find((c) => c.key === sort.key);
  if (!col?.sort) return rows;
  const acc = col.sort;
  const dir = sort.dir === "asc" ? 1 : -1;
  return [...rows].sort((a, b) => {
    const av = acc(a);
    const bv = acc(b);
    if (av == null && bv == null) return 0;
    if (av == null) return 1;
    if (bv == null) return -1;
    if (typeof av === "number" && typeof bv === "number") return (av - bv) * dir;
    return String(av).localeCompare(String(bv), undefined, { numeric: true }) * dir;
  });
}

export function DataTable<Row>({
  rows,
  columns,
  rowKey,
  loading,
  error,
  empty,
  onRowClick,
  rowClassName,
  sort,
  onSortChange,
  dense,
  className,
  selectedKey,
  footer,
  skeletonRows = 6,
}: DataTableProps<Row>) {
  const t = useT();
  const cols = columns.filter((c) => !c.hidden);
  const sorted = rows ? sortRows(rows, cols, sort) : [];
  const alignCls = (a?: "left" | "right" | "center") => (a === "right" ? "text-right" : a === "center" ? "text-center" : "text-left");
  const cellPad = dense ? "px-3 py-1.5" : "px-3 py-2";

  return (
    <div className={cn("overflow-x-auto subtle-scroll", className)}>
      <table className="w-full text-[13px] border-collapse">
        <thead>
          <tr className="border-b border-border">
            {cols.map((c) => {
              const active = sort?.key === c.key;
              const sortable = !!c.sort && !!onSortChange;
              return (
                <th
                  key={c.key}
                  scope="col"
                  style={c.width ? { width: c.width } : undefined}
                  className={cn(
                    "px-3 py-2 text-[11px] font-medium uppercase tracking-wide text-fg-faint whitespace-nowrap select-none",
                    alignCls(c.align),
                    sortable && "cursor-pointer hover:text-fg-muted",
                    c.headerClassName,
                  )}
                  onClick={
                    sortable
                      ? () => {
                          if (!active) onSortChange({ key: c.key, dir: "asc" });
                          else if (sort.dir === "asc") onSortChange({ key: c.key, dir: "desc" });
                          else onSortChange(null);
                        }
                      : undefined
                  }
                  aria-sort={active ? (sort.dir === "asc" ? "ascending" : "descending") : undefined}
                >
                  <span className={cn("inline-flex items-center gap-1", c.align === "right" && "flex-row-reverse")}>
                    {c.header}
                    {sortable ? (
                      active ? (
                        sort.dir === "asc" ? (
                          <ChevronUp className="size-3" />
                        ) : (
                          <ChevronDown className="size-3" />
                        )
                      ) : (
                        <ChevronsUpDown className="size-3 opacity-50" />
                      )
                    ) : null}
                  </span>
                </th>
              );
            })}
          </tr>
        </thead>
        <tbody>
          {loading && !rows
            ? Array.from({ length: skeletonRows }).map((_, i) => (
                <tr key={i} className="border-b border-border last:border-0">
                  {cols.map((c) => (
                    <td key={c.key} className={cellPad}>
                      <Skeleton className="h-3.5 w-[60%]" />
                    </td>
                  ))}
                </tr>
              ))
            : null}
          {!loading && error ? (
            <tr>
              <td colSpan={cols.length}>
                {isValidElement(error) ? error : <QueryError compact error={error} />}
              </td>
            </tr>
          ) : null}
          {rows && !error && sorted.length === 0 ? (
            <tr>
              <td colSpan={cols.length}>{empty ?? <EmptyState compact title={t("common.empty")} />}</td>
            </tr>
          ) : null}
          {sorted.map((row) => {
            const k = rowKey(row);
            return (
              <tr
                key={k}
                data-testid="row"
                data-row-key={k}
                onClick={onRowClick ? () => onRowClick(row) : undefined}
                className={cn(
                  "border-b border-border last:border-0 transition-colors",
                  onRowClick && "cursor-pointer hover:bg-surface-2/70",
                  selectedKey === k && "bg-surface-2",
                  rowClassName?.(row),
                )}
              >
                {cols.map((c) => (
                  <td key={c.key} className={cn(cellPad, alignCls(c.align), "align-middle", c.className)}>
                    {c.cell(row)}
                  </td>
                ))}
              </tr>
            );
          })}
        </tbody>
        {footer ? (
          <tfoot>
            <tr>
              <td colSpan={cols.length}>{footer}</td>
            </tr>
          </tfoot>
        ) : null}
      </table>
    </div>
  );
}

/** Cursor / offset pager shared by tables. */
export function Pager({
  page,
  pages,
  onPage,
  hasPrev,
  hasNext,
  total,
  totalLabel,
  pageSize,
  onPageSize,
  className,
}: {
  page?: number;
  pages?: number;
  onPage: (dir: -1 | 1) => void;
  hasPrev: boolean;
  hasNext: boolean;
  total?: number;
  totalLabel?: ReactNode;
  /** Rows per page; renders a selector when `onPageSize` is given. */
  pageSize?: number;
  onPageSize?: (n: number) => void;
  className?: string;
}) {
  const t = useT();
  return (
    <div className={cn("flex items-center justify-between gap-3 px-3 py-2 border-t border-border text-xs text-fg-muted", className)}>
      <span>{totalLabel ?? (total !== undefined ? t("common.total", { n: total }) : null)}</span>
      <div className="flex items-center gap-1">
        {pageSize !== undefined && onPageSize ? (
          <label className="mr-3 inline-flex items-center gap-1.5 whitespace-nowrap">
            <span>{t("common.perPage")}</span>
            <NativeSelect
              aria-label={t("common.perPage")}
              data-testid="page-size"
              className="h-7 w-[4.25rem] pl-2 text-xs"
              value={String(pageSize)}
              onChange={(e) => onPageSize(Number(e.target.value))}
            >
              {PAGE_SIZES.map((n) => (
                <option key={n} value={n}>
                  {n}
                </option>
              ))}
            </NativeSelect>
          </label>
        ) : null}
        {page !== undefined && pages !== undefined ? <span className="tabular mr-2">{t("common.page", { page, pages })}</span> : null}
        <Button variant="ghost" size="icon" className="h-7 w-7" disabled={!hasPrev} onClick={() => onPage(-1)} aria-label={t("common.previous")}>
          <ChevronLeft className="size-4" />
        </Button>
        <Button variant="ghost" size="icon" className="h-7 w-7" disabled={!hasNext} onClick={() => onPage(1)} aria-label={t("common.next")}>
          <ChevronRight className="size-4" />
        </Button>
      </div>
    </div>
  );
}
