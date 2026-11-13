// Placeholder cards shaped like the list rows, while a list loads.
export function SkeletonList({ rows = 3 }: { rows?: number }) {
  return (
    <div role="status" className="grid gap-3">
      <span className="sr-only">loading…</span>
      {Array.from({ length: rows }, (_, i) => (
        <div key={i} className="card px-5 py-4" aria-hidden="true">
          <div
            className="h-5 w-40 max-w-full animate-pulse rounded"
            style={{ backgroundColor: "var(--color-surface-muted)" }}
          />
          <div
            className="mt-2.5 h-3 w-72 max-w-full animate-pulse rounded"
            style={{ backgroundColor: "var(--color-surface-muted)" }}
          />
        </div>
      ))}
    </div>
  );
}
