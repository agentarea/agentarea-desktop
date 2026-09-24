import { useMemo, useState } from 'react';
import { ChevronDown, ChevronRight } from 'lucide-react';
import { cn } from '@/lib/utils';
import type { Message } from '@/data';
import { FileChip, localRef, useThreadFolder } from './FileChip';

/** Tools whose calls show as file-change rows instead of inside "Used N tools". */
const FILE_TOOLS = new Set(['Write', 'Edit', 'MultiEdit', 'NotebookEdit', 'fileChange']);
export const isFileChange = (m: Message) => m.role === 'tool' && !!m.tool && FILE_TOOLS.has(m.tool);

type Line = { kind: '+' | '-' | ' ' | '@'; text: string };

interface Change {
  path: string;
  verb: 'Created' | 'Edited' | 'Deleted';
  lines: Line[];
}

const splitLines = (s: string) => (s ? s.replace(/\n$/, '').split('\n') : []);

/** Line diff of two snippets (LCS); big inputs fall back to all-removed / all-added. */
function diffLines(before: string, after: string): Line[] {
  const a = splitLines(before);
  const b = splitLines(after);
  if (a.length * b.length > 250_000)
    return [...a.map((text) => ({ kind: '-' as const, text })), ...b.map((text) => ({ kind: '+' as const, text }))];
  const lcs = Array.from({ length: a.length + 1 }, () => new Uint32Array(b.length + 1));
  for (let i = a.length - 1; i >= 0; i--)
    for (let j = b.length - 1; j >= 0; j--)
      lcs[i][j] = a[i] === b[j] ? lcs[i + 1][j + 1] + 1 : Math.max(lcs[i + 1][j], lcs[i][j + 1]);
  const out: Line[] = [];
  let i = 0;
  let j = 0;
  while (i < a.length || j < b.length) {
    if (i < a.length && j < b.length && a[i] === b[j]) {
      out.push({ kind: ' ', text: a[i] });
      i++;
      j++;
    } else if (i < a.length && (j >= b.length || lcs[i + 1][j] >= lcs[i][j + 1])) out.push({ kind: '-', text: a[i++] });
    else out.push({ kind: '+', text: b[j++] });
  }
  return out;
}

/** A unified diff (codex `fileChange`): drop file headers, keep hunks. */
function parseUnified(diff: string): Line[] {
  const out: Line[] = [];
  for (const l of splitLines(diff)) {
    if (/^(diff |index |--- |\+\+\+ |\\ No newline)/.test(l)) continue;
    if (l.startsWith('@@')) out.push({ kind: '@', text: l });
    else if (l[0] === '+' || l[0] === '-') out.push({ kind: l[0], text: l.slice(1) });
    else out.push({ kind: ' ', text: l.startsWith(' ') ? l.slice(1) : l });
  }
  return out;
}

function parse(text: string): any {
  try {
    return JSON.parse(text);
  } catch {
    return null;
  }
}

/** What a file tool call changed, derived from its raw input (and, for Write, its result). */
function changesOf(m: Message): Change[] {
  const input = parse(m.text);
  if (!input) return [];
  if (m.tool === 'fileChange')
    return (Array.isArray(input) ? input : []).map((c: any): Change => {
      const kind = c?.kind?.type ?? c?.kind;
      const diff = String(c?.diff ?? '');
      const unified = /^(@@|--- |diff )/m.test(diff);
      return {
        path: String(c?.path ?? ''),
        verb: kind === 'add' ? 'Created' : kind === 'delete' ? 'Deleted' : 'Edited',
        lines: unified
          ? parseUnified(diff)
          : splitLines(diff).map((text) => ({ kind: kind === 'delete' ? '-' : '+', text })),
      };
    });
  const path = String(input.file_path ?? input.notebook_path ?? '');
  switch (m.tool) {
    case 'Write':
      return [
        {
          path,
          verb: /updated|overwr/i.test(m.result ?? '') ? 'Edited' : 'Created',
          lines: splitLines(String(input.content ?? '')).map((text) => ({ kind: '+', text })),
        },
      ];
    case 'Edit':
      return [{ path, verb: 'Edited', lines: diffLines(String(input.old_string ?? ''), String(input.new_string ?? '')) }];
    case 'MultiEdit':
      return [
        {
          path,
          verb: 'Edited',
          lines: (input.edits ?? []).flatMap((e: any, k: number): Line[] => [
            ...(k ? [{ kind: '@' as const, text: '⋯' }] : []),
            ...diffLines(String(e?.old_string ?? ''), String(e?.new_string ?? '')),
          ]),
        },
      ];
    case 'NotebookEdit':
      return [
        {
          path,
          verb: input.edit_mode === 'delete' ? 'Deleted' : 'Edited',
          lines: splitLines(String(input.new_source ?? '')).map((text) => ({ kind: '+', text })),
        },
      ];
  }
  return [];
}

const MAX_LINES = 300;

function DiffView({ lines, dense }: { lines: Line[]; dense?: boolean }) {
  const shown = lines.slice(0, MAX_LINES);
  return (
    <div className="mt-1 mb-0.5 overflow-hidden rounded-lg border border-border bg-background">
      <pre
        className={cn(
          'max-h-80 overflow-auto py-1 font-mono leading-[1.55] scrollbar-sleek',
          dense ? 'text-[10.5px]' : 'text-[11.5px]',
        )}
      >
        {shown.map((l, i) => (
          <div
            key={i}
            className={cn(
              'flex min-w-max pr-3',
              l.kind === '+' && 'bg-emerald-500/10 text-emerald-800 dark:text-emerald-300',
              l.kind === '-' && 'bg-red-500/10 text-red-800 dark:text-red-300',
              l.kind === '@' && 'text-muted-foreground/70',
              l.kind === ' ' && 'text-foreground/75',
            )}
          >
            <span className="w-6 shrink-0 text-center opacity-60 select-none">{l.kind === '@' ? '' : l.kind}</span>
            <span className="whitespace-pre">{l.text || ' '}</span>
          </div>
        ))}
        {lines.length > MAX_LINES && (
          <div className="px-6 pt-1 text-muted-foreground italic">… {lines.length - MAX_LINES} more lines</div>
        )}
      </pre>
    </div>
  );
}

function ChangeRow({ change, status, dense }: { change: Change; status: 'running' | 'failed' | null; dense?: boolean }) {
  const [open, setOpen] = useState(false);
  const folder = useThreadFolder();
  const file = localRef(change.path, folder) ?? { path: change.path };
  const added = change.lines.filter((l) => l.kind === '+').length;
  const removed = change.lines.filter((l) => l.kind === '-').length;
  const Chevron = open ? ChevronDown : ChevronRight;
  return (
    <div className={cn('min-w-0', dense ? 'text-[11.5px]' : 'text-[13px]')}>
      <div
        role="button"
        tabIndex={0}
        onClick={() => setOpen(!open)}
        onKeyDown={(e) => (e.key === 'Enter' || e.key === ' ') && (e.preventDefault(), setOpen(!open))}
        className="group flex min-w-0 cursor-pointer items-baseline gap-1.5 text-muted-foreground select-none hover:text-foreground"
      >
        <span className="shrink-0">{status === 'running' ? change.verb.replace(/ed$/, 'ing') : change.verb}</span>
        <FileChip file={file} className="min-w-0" />
        {added > 0 && <span className="shrink-0 font-mono text-[0.92em] text-emerald-600 dark:text-emerald-400">+{added}</span>}
        {removed > 0 && <span className="shrink-0 font-mono text-[0.92em] text-red-600 dark:text-red-400">−{removed}</span>}
        {status === 'running' && <span className="size-1 shrink-0 animate-pulse rounded-full bg-chart-2" />}
        {status === 'failed' && <span className="shrink-0 text-[0.92em] text-destructive">failed</span>}
        <Chevron className="size-3 shrink-0 self-center opacity-0 transition-opacity group-hover:opacity-60" />
      </div>
      {open && change.lines.length > 0 && <DiffView lines={change.lines} dense={dense} />}
    </div>
  );
}

/** A Write / Edit / fileChange call as Codex shows it: "Edited App.tsx +3 −1", expandable to the diff. */
export function FileChangeRow({ message: m, dense }: { message: Message; dense?: boolean }) {
  const changes = useMemo(() => changesOf(m), [m.text, m.tool, m.result]);
  const r = m.result;
  const status =
    r === undefined ? 'running' : /tool_use_error|^error|^failed|^declined|rejected|denied|doesn't want to proceed/i.test(r.trim()) ? 'failed' : null;
  if (!changes.length) return null;
  return (
    <div data-file-change className={cn('flex min-w-0 flex-col gap-1', dense ? '[[data-file-change]+&]:-mt-1.5' : '[[data-file-change]+&]:-mt-3.5')}>
      {changes.map((c, i) => (
        <ChangeRow key={i} change={c} status={status} dense={dense} />
      ))}
    </div>
  );
}
