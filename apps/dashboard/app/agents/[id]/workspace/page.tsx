"use client";

import { Suspense, use, useCallback, useEffect, useRef, useState } from "react";
import Link from "next/link";
import { useRouter, useSearchParams } from "next/navigation";
import { Download, File, Folder, Link2 } from "lucide-react";
import {
  ApiError,
  api,
  workspaceFileUrl,
  type WorkspaceEntry,
  type WorkspaceListing,
} from "@/lib/api";

/** Largest text file shown in the page. */
const MAX_PREVIEW_BYTES = 512 * 1024;
const IMAGE = /\.(png|jpe?g|gif|webp)$/i;

function sizeText(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

function timeText(seconds?: number): string {
  return seconds ? new Date(seconds * 1000).toLocaleString() : "";
}

/** What is shown of a file. */
type Preview =
  | { path: string; kind: "text"; text: string }
  | { path: string; kind: "image" }
  | { path: string; kind: "none"; why: string };

/**
 * `/agents/[id]/workspace` — the files of the agent's workspace, read-only:
 * folders, text and images shown, any file downloaded. Links are shown and
 * never followed.
 */
export default function WorkspacePage({ params }: { params: Promise<{ id: string }> }) {
  const { id } = use(params);
  return (
    <Suspense>
      <AddressedFolder id={id} />
    </Suspense>
  );
}

/** The folder the address names; a new folder starts afresh. */
function AddressedFolder({ id }: { id: string }) {
  const path = useSearchParams().get("path") ?? "";
  return <Workspace key={path} id={id} path={path} />;
}

function Workspace({ id, path }: { id: string; path: string }) {
  const router = useRouter();
  const [listing, setListing] = useState<WorkspaceListing | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [preview, setPreview] = useState<Preview | null>(null);
  // The file clicked last: an answer for one clicked before is dropped.
  const shown = useRef<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    api.workspace.list(id, path).then(
      (l) => {
        if (!cancelled) setListing(l);
      },
      (e: unknown) => {
        if (cancelled) return;
        if (e instanceof ApiError && e.status === 403) {
          setError("Workspace browsing is off for this agent; it is turned on in its settings.");
        } else {
          setError(e instanceof Error ? e.message : String(e));
        }
      },
    );
    return () => {
      cancelled = true;
    };
  }, [id, path]);

  const go = useCallback(
    (to: string) =>
      router.push(`/agents/${id}/workspace${to ? `?path=${encodeURIComponent(to)}` : ""}`),
    [id, router],
  );

  async function show(entry: WorkspaceEntry) {
    const full = path ? `${path}/${entry.name}` : entry.name;
    shown.current = full;
    const settle = (p: Preview) => {
      if (shown.current === full) setPreview(p);
    };
    if (IMAGE.test(entry.name)) {
      setPreview({ path: full, kind: "image" });
      return;
    }
    if ((entry.size ?? 0) > MAX_PREVIEW_BYTES) {
      setPreview({ path: full, kind: "none", why: "too large to show here" });
      return;
    }
    try {
      const resp = await fetch(workspaceFileUrl(id, full), { credentials: "same-origin" });
      if (!resp.ok) throw new Error(`${resp.status} ${resp.statusText}`);
      const bytes = new Uint8Array(await resp.arrayBuffer());
      // It may have grown since it was listed.
      if (bytes.length > MAX_PREVIEW_BYTES) {
        settle({ path: full, kind: "none", why: "too large to show here" });
        return;
      }
      if (bytes.subarray(0, 8192).includes(0)) {
        settle({ path: full, kind: "none", why: "not text" });
        return;
      }
      settle({ path: full, kind: "text", text: new TextDecoder().decode(bytes) });
    } catch (e) {
      settle({ path: full, kind: "none", why: e instanceof Error ? e.message : String(e) });
    }
  }

  const crumbs = path ? path.split("/") : [];
  const muted = { color: "var(--color-body)" };
  return (
    <div className="mx-auto max-w-5xl px-6 py-8">
      <p className="mb-2 text-sm">
        <Link href={`/agents/${id}`} className="underline">
          back to the chat
        </Link>
      </p>
      <h1 className="display-h2 mb-4">workspace</h1>
      <nav aria-label="folder" className="mb-4 flex flex-wrap items-center gap-1 font-mono text-sm">
        <button type="button" className="underline" onClick={() => go("")}>
          workspace
        </button>
        {crumbs.map((name, i) => (
          <span key={i} className="inline-flex items-center gap-1">
            <span style={muted}>/</span>
            <button
              type="button"
              className="underline"
              onClick={() => go(crumbs.slice(0, i + 1).join("/"))}
            >
              {name}
            </button>
          </span>
        ))}
      </nav>
      {error && (
        <p className="mb-4 text-sm" role="alert" style={{ color: "var(--color-ruby)" }}>
          {error}
        </p>
      )}
      {listing && listing.entries.length === 0 && (
        <p className="text-sm" style={muted}>
          This folder is empty.
        </p>
      )}
      {listing && listing.entries.length > 0 && (
        <table className="w-full text-sm" aria-label="files">
          <thead>
            <tr className="text-left" style={muted}>
              <th className="py-1 font-normal">name</th>
              <th className="py-1 font-normal">size</th>
              <th className="py-1 font-normal">modified</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {listing.entries.map((entry) => {
              const full = path ? `${path}/${entry.name}` : entry.name;
              return (
                <tr
                  key={entry.name}
                  className="border-t"
                  style={{ borderColor: "var(--color-hairline)" }}
                >
                  <td className="py-1.5 pr-3 wrap-anywhere">
                    {entry.kind === "dir" && (
                      <button
                        type="button"
                        className="inline-flex items-center gap-1.5 underline"
                        onClick={() => go(full)}
                      >
                        <Folder size={14} aria-hidden="true" /> {entry.name}/
                      </button>
                    )}
                    {entry.kind === "file" && (
                      <button
                        type="button"
                        className="inline-flex items-center gap-1.5 underline"
                        onClick={() => void show(entry)}
                      >
                        <File size={14} aria-hidden="true" /> {entry.name}
                      </button>
                    )}
                    {entry.kind === "link" && (
                      <span
                        className="inline-flex items-center gap-1.5"
                        title="a link: shown, never followed"
                      >
                        <Link2 size={14} aria-hidden="true" /> {entry.name}
                        <span className="font-mono text-xs" style={muted}>
                          &rarr; {entry.target ?? "?"} (link, not followed)
                        </span>
                      </span>
                    )}
                    {entry.kind === "other" && <span style={muted}>{entry.name}</span>}
                  </td>
                  <td className="py-1.5 pr-3 tabular" style={muted}>
                    {entry.size !== undefined ? sizeText(entry.size) : ""}
                  </td>
                  <td className="py-1.5 pr-3" style={muted}>
                    {timeText(entry.modified)}
                  </td>
                  <td className="py-1.5 text-right">
                    {entry.kind === "file" && (
                      <a
                        href={workspaceFileUrl(id, full)}
                        download={entry.name}
                        className="inline-flex items-center gap-1 underline"
                        aria-label={`download ${entry.name}`}
                      >
                        <Download size={14} aria-hidden="true" /> download
                      </a>
                    )}
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      )}
      {listing?.truncated && (
        <p className="mt-2 text-xs" style={muted}>
          This folder has more entries than are listed here.
        </p>
      )}
      {preview && (
        <section className="mt-6 card px-4 py-4" aria-label="preview">
          <p className="mb-2 font-mono text-sm wrap-anywhere">{preview.path}</p>
          {preview.kind === "text" && (
            <pre className="max-h-[60vh] overflow-auto whitespace-pre-wrap break-words text-xs">
              {preview.text}
            </pre>
          )}
          {preview.kind === "image" && (
            // eslint-disable-next-line @next/next/no-img-element
            <img
              src={workspaceFileUrl(id, preview.path)}
              alt={preview.path}
              className="max-h-[60vh]"
            />
          )}
          {preview.kind === "none" && (
            <p className="text-sm" style={muted}>
              Not shown here ({preview.why}); download it instead.
            </p>
          )}
        </section>
      )}
    </div>
  );
}
