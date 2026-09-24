import { Children, isValidElement, memo, type ComponentProps, type ReactElement, type ReactNode } from 'react';
import ReactMarkdown, { defaultUrlTransform, type Components } from 'react-markdown';
import remarkGfm from 'remark-gfm';
import { cn } from '@/lib/utils';
import { FileChip, localRef, openExternal, useThreadFolder } from './FileChip';

const textOf = (node: ReactNode): string =>
  Children.toArray(node)
    .map((c) => (typeof c === 'string' || typeof c === 'number' ? String(c) : isValidElement(c) ? textOf((c.props as { children?: ReactNode }).children) : ''))
    .join('');

/** Links: local paths become file chips, web links open in the browser (never in the webview). */
function Link({ href = '', children }: ComponentProps<'a'>) {
  const folder = useThreadFolder();
  const file = localRef(href, folder);
  if (file) {
    const label = textOf(children).trim();
    // A label that is itself a path (or the raw href) shows as the basename; prose labels stay.
    const pathy = !label || label.includes('/') || label === href || /^[\w.@~-]+\.\w+(:\d+)?$/.test(label);
    return <FileChip file={file} label={pathy ? undefined : label} />;
  }
  const web = /^(https?|mailto):/i.test(href);
  return (
    <a
      href={href}
      title={web ? href : undefined}
      onClick={(e) => {
        e.preventDefault();
        if (web) void openExternal(href);
      }}
      className="font-medium text-foreground underline decoration-foreground/25 underline-offset-2 hover:decoration-foreground/70"
    >
      {children}
    </a>
  );
}

/** `inline code` — or a chip when it is an unambiguous absolute path inside the thread folder. */
function InlineCode({ children }: ComponentProps<'code'>) {
  const folder = useThreadFolder();
  const text = textOf(children);
  if (folder && text.startsWith(folder.replace(/\/$/, '') + '/') && !/\s/.test(text)) {
    const file = localRef(text, folder);
    if (file) return <FileChip file={file} />;
  }
  return <code className="rounded bg-muted px-1 py-0.5 font-mono text-[0.88em] text-foreground">{children}</code>;
}

/** Fenced code: the monospace pane, with the language as a small label. */
function CodeBlock({ children }: ComponentProps<'pre'>) {
  const code = Children.toArray(children)[0] as ReactElement<{ className?: string; children?: ReactNode }> | undefined;
  const lang = /language-([\w+#.-]+)/.exec(code?.props?.className ?? '')?.[1];
  const text = textOf(code?.props?.children ?? children).replace(/\n$/, '');
  return (
    <div className="my-2 overflow-hidden rounded-lg bg-muted">
      {lang && <div className="px-3 pt-1.5 text-[10.5px] font-medium text-muted-foreground/80">{lang}</div>}
      <pre className={cn('max-w-full overflow-x-auto px-3 font-mono text-[0.86em] leading-normal text-foreground/90', lang ? 'pt-1 pb-2' : 'py-2')}>
        <code>{text}</code>
      </pre>
    </div>
  );
}

const components: Components = {
  a: Link,
  code: InlineCode,
  pre: CodeBlock,
  p: ({ children }) => <p className="my-2 first:mt-0 last:mb-0">{children}</p>,
  h1: ({ children }) => <h1 className="mt-4 mb-1.5 text-[1.2em] font-semibold first:mt-0">{children}</h1>,
  h2: ({ children }) => <h2 className="mt-4 mb-1.5 text-[1.1em] font-semibold first:mt-0">{children}</h2>,
  h3: ({ children }) => <h3 className="mt-3 mb-1 text-[1em] font-semibold first:mt-0">{children}</h3>,
  h4: ({ children }) => <h4 className="mt-3 mb-1 text-[1em] font-medium first:mt-0">{children}</h4>,
  h5: ({ children }) => <h5 className="mt-3 mb-1 font-medium first:mt-0">{children}</h5>,
  h6: ({ children }) => <h6 className="mt-3 mb-1 font-medium text-muted-foreground first:mt-0">{children}</h6>,
  ul: ({ children, className }) => (
    <ul className={cn('my-2 pl-5 first:mt-0 last:mb-0 [&_ol]:my-0.5 [&_ul]:my-0.5', className?.includes('contains-task-list') ? 'list-none pl-1' : 'list-disc')}>
      {children}
    </ul>
  ),
  ol: ({ children, start }) => (
    <ol start={start} className="my-2 list-decimal pl-5 first:mt-0 last:mb-0 [&_ol]:my-0.5 [&_ul]:my-0.5">
      {children}
    </ol>
  ),
  li: ({ children }) => <li className="my-0.5 pl-0.5 marker:text-muted-foreground [&>p]:my-1">{children}</li>,
  blockquote: ({ children }) => (
    <blockquote className="my-2 border-l-2 border-border pl-3 text-muted-foreground">{children}</blockquote>
  ),
  hr: () => <hr className="my-4 border-border" />,
  table: ({ children }) => (
    <div className="my-2 max-w-full overflow-x-auto rounded-lg border border-border">
      <table className="w-full border-collapse text-[0.93em]">{children}</table>
    </div>
  ),
  th: ({ children, style }) => (
    <th style={style} className="border-b border-border bg-muted/60 px-2.5 py-1.5 text-left font-medium">
      {children}
    </th>
  ),
  td: ({ children, style }) => (
    <td style={style} className="border-b border-border px-2.5 py-1.5 align-top [tr:last-child>&]:border-b-0">
      {children}
    </td>
  ),
  img: ({ src, alt }) => (typeof src === 'string' ? <Link href={src}>{alt || src}</Link> : null),
  input: ({ checked }) => <input type="checkbox" checked={!!checked} readOnly className="mr-1.5 translate-y-[1px]" />,
};

const remarkPlugins = [remarkGfm];
const urlTransform = (url: string) => (/^file:/i.test(url) ? url : defaultUrlTransform(url));

/** Agent markdown, Codex style: compact typography, file chips, code panes. Memoized on the text. */
export const Markdown = memo(function Markdown({ text, className }: { text: string; className?: string }) {
  return (
    <div className={cn('min-w-0 break-words', className)}>
      <ReactMarkdown remarkPlugins={remarkPlugins} components={components} urlTransform={urlTransform}>
        {text}
      </ReactMarkdown>
    </div>
  );
});
