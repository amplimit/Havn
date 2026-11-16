"use client";

/**
 * Markdown renderer for chat bubbles + composer preview.
 *
 * Plugins:
 * - remark-gfm: tables, task lists, strikethrough, autolinks
 * - remark-math + rehype-katex: $inline$ and $$display$$ LaTeX math
 *
 * No rehype-sanitize: react-markdown 9+ refuses to render raw HTML
 * in source by default (you'd need rehype-raw to opt in), so the
 * `<script>` injection vector is closed at the source. The KaTeX
 * output is HTML emitted by code we control, not from the markdown
 * source. Adding sanitize back would mean carving a permissive
 * schema for KaTeX's hundreds of class names — net negative.
 *
 * One Markdown component used in two places (Bubble + composer
 * preview) keeps style + features consistent.
 */

import { memo } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import remarkMath from "remark-math";
import rehypeKatex from "rehype-katex";
import "katex/dist/katex.min.css";

// Memoized: the chat renders on every piece of a reply being written,
// and its earlier messages do not change.
export const Markdown = memo(function Markdown({ children }: { children: string }) {
  return (
    <div className="markdown">
      <ReactMarkdown
        remarkPlugins={[remarkGfm, remarkMath]}
        rehypePlugins={[rehypeKatex]}
        components={{
          a: ({ href, children, ...props }) => (
            <a
              href={href}
              target="_blank"
              rel="noopener noreferrer"
              {...props}
            >
              {children}
            </a>
          ),
        }}
      >
        {children}
      </ReactMarkdown>
    </div>
  );
});
