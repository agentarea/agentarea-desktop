import { useState, type ReactNode } from 'react';
import { ChevronDown, ChevronRight, ShieldQuestion, Wrench } from 'lucide-react';
import { cn } from '@/lib/utils';
import type { Message } from '@/data';
import type { Approval, Decision } from '@/lib/local';
import { useAppStore } from '@/store';
import { FileChangeRow, isFileChange } from './FileChange';
import { Markdown } from './Markdown';

/**
 * Consecutive tool calls collapse into one "Used N tools" row, as in Codex.
 * File writes/edits stay out of it: they show as their own "Edited x.ts" rows.
 */
export function groupMessages(messages: Message[]): (Message | Message[])[] {
  const out: (Message | Message[])[] = [];
  for (const m of messages) {
    const last = out[out.length - 1];
    const grouped = m.role === 'tool' && !isFileChange(m);
    if (grouped && Array.isArray(last)) last.push(m);
    else out.push(grouped ? [m] : m);
  }
  return out;
}

function prettyPrint(text: string): string {
  try {
    return JSON.stringify(JSON.parse(text), null, 2);
  } catch {
    return text;
  }
}

export function ToolGroup({ tools, dense }: { tools: Message[]; dense?: boolean }) {
  const [open, setOpen] = useState(false);
  return (
    <div className={cn('text-muted-foreground', dense ? 'text-[11px]' : 'text-[12.5px]')}>
      <button onClick={() => setOpen(!open)} className="flex items-center gap-1 hover:text-foreground">
        {open ? <ChevronDown className="size-3" /> : <ChevronRight className="size-3" />}
        Used {tools.length} {tools.length === 1 ? 'tool' : 'tools'}
      </button>
      {open && (
        <div className="mt-1.5 flex flex-col border-l border-border pl-3">
          {tools.map((m) => (
            <ToolRow key={m.id} message={m} />
          ))}
        </div>
      )}
    </div>
  );
}

function DetailLabel({ children }: { children: ReactNode }) {
  return <div className="mb-0.5 text-[10px] font-medium tracking-wide text-muted-foreground/70 uppercase">{children}</div>;
}

function ToolDetail({ label, text }: { label: string; text: string }) {
  return (
    <div>
      <DetailLabel>{label}</DetailLabel>
      <pre className="max-h-40 overflow-auto rounded-md bg-background px-2 py-1.5 font-mono text-[11px] whitespace-pre-wrap break-all text-foreground/85">
        {text}
      </pre>
    </div>
  );
}

/** A tool row expands to its pretty-printed input and, once it arrives, its result. */
function ToolRow({ message: m }: { message: Message }) {
  const [open, setOpen] = useState(false);
  return (
    <div className="min-w-0 py-0.5">
      <button onClick={() => setOpen(!open)} className="flex w-full min-w-0 items-center gap-1.5 text-left hover:text-foreground">
        <Wrench className="size-3 shrink-0 text-ai-action-accent" />
        <code className="shrink-0 font-mono text-foreground">{m.tool}</code>
        <span className="truncate font-mono">{m.text}</span>
        {open ? (
          <ChevronDown className="ml-auto size-3 shrink-0 opacity-60" />
        ) : (
          <ChevronRight className="ml-auto size-3 shrink-0 opacity-60" />
        )}
      </button>
      {open && (
        <div className="mt-1 mb-1 flex flex-col gap-1.5 rounded-lg border border-border bg-muted/50 p-2">
          <ToolDetail label="Input" text={prettyPrint(m.text)} />
          {m.result !== undefined ? (
            <ToolDetail label="Result" text={m.result} />
          ) : (
            <div>
              <DetailLabel>Result</DetailLabel>
              <div className="flex items-center gap-1.5 rounded-md bg-background px-2 py-1.5 text-[11px] text-muted-foreground italic">
                <span className="size-1 shrink-0 animate-pulse rounded-full bg-chart-2" />
                running…
              </div>
            </div>
          )}
        </div>
      )}
    </div>
  );
}

export function MessageRow({ message: m, dense }: { message: Message; dense?: boolean }) {
  if (m.role === 'tool') return <FileChangeRow message={m} dense={dense} />;
  if (m.role === 'user')
    return (
      <div
        className={cn(
          'ml-auto max-w-[85%] whitespace-pre-wrap rounded-3xl bg-secondary text-foreground',
          dense ? 'px-3 py-1.5 text-[12.5px]' : 'px-4 py-2.5 text-[14px] leading-relaxed',
        )}
      >
        {m.text}
      </div>
    );
  if (m.role === 'note')
    return <div className={cn('text-muted-foreground italic', dense ? 'text-[11.5px]' : 'text-[12.5px]')}>{m.text}</div>;
  if (m.role === 'error')
    return (
      <div className={cn('whitespace-pre-wrap text-destructive', dense ? 'text-[12px]' : 'text-[14px] leading-relaxed')}>
        {m.text}
      </div>
    );
  return <Markdown text={m.text} className={cn('text-foreground/90', dense ? 'text-[12.5px] leading-normal' : 'text-[14.5px] leading-relaxed')} />;
}

/** The reply as it streams in, before it lands as a message. */
export function DraftRow({ text, dense }: { text: string; dense?: boolean }) {
  return (
    <Markdown
      text={text}
      className={cn('aa-caret text-foreground/90', dense ? 'text-[12.5px] leading-normal' : 'text-[14.5px] leading-relaxed')}
    />
  );
}

/** Allow / Deny for something the agent waits on, inline in the thread (and in the inbox). */
export function ApprovalActions({ approval, compact }: { approval: Approval; compact?: boolean }) {
  const approve = useAppStore((s) => s.approve);
  const [busy, setBusy] = useState(false);
  const decide = (d: Decision) => {
    setBusy(true);
    void approve(approval.session_id, approval.id, d);
  };
  const size = compact ? 'px-2.5 py-0.5 text-[11.5px]' : 'px-3 py-1 text-[12px]';
  return (
    <div className="flex flex-wrap items-center gap-1.5" onClick={(e) => e.stopPropagation()}>
      <button
        disabled={busy}
        onClick={() => decide('allow')}
        className={cn('rounded-full bg-foreground font-medium text-background disabled:opacity-50', size)}
      >
        Allow
      </button>
      {approval.can_session && (
        <button
          disabled={busy}
          onClick={() => decide('allow_session')}
          className={cn('rounded-full border border-border text-foreground hover:bg-accent disabled:opacity-50', size)}
        >
          Allow for session
        </button>
      )}
      <button
        disabled={busy}
        onClick={() => decide('deny')}
        className={cn('rounded-full border border-border text-foreground hover:bg-accent disabled:opacity-50', size)}
      >
        Deny
      </button>
    </div>
  );
}

export function ApprovalCard({ approval, dense }: { approval: Approval; dense?: boolean }) {
  return (
    <div className={cn('flex flex-col gap-2 rounded-2xl border border-amber-500/30 bg-amber-500/[0.04]', dense ? 'p-2.5' : 'p-3.5')}>
      <div className={cn('flex items-center gap-1.5 font-medium text-foreground', dense ? 'text-[12px]' : 'text-[13px]')}>
        <ShieldQuestion className="size-3.5 text-amber-600" />
        {approval.title}
      </div>
      {approval.detail && (
        <pre
          className={cn(
            'max-h-40 overflow-auto rounded-lg bg-background px-2.5 py-1.5 font-mono whitespace-pre-wrap break-all text-foreground/85',
            dense ? 'text-[11px]' : 'text-[12px]',
          )}
        >
          {approval.detail}
        </pre>
      )}
      {approval.reason && <div className="text-[12px] text-muted-foreground">{approval.reason}</div>}
      <ApprovalActions approval={approval} compact={dense} />
    </div>
  );
}
