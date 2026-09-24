import { useEffect, useRef, useState } from 'react';
import { ArrowUp, Plus, Square } from 'lucide-react';
import { cn } from '@/lib/utils';
import { useAppStore } from '@/store';
import type { Session } from '@/data';
import { RunnerBadge } from '@/assets/icons';
import { basename } from './format';
import { ThreadFolder } from './FileChip';
import { ApprovalCard, DraftRow, groupMessages, MessageRow, ToolGroup } from './MessageView';
import { AttachmentChips } from './Composer';
import { DropOverlay } from './DropZone';

// Enough context to reply well from a tile, without rendering a whole thread's history.
const TILE_TRANSCRIPT_SIZE = 14;

/** Pane count → CSS grid: 2 or 3 side by side, 4 as a 2×2. */
const LAYOUT: Record<2 | 3 | 4, string> = {
  2: 'grid-cols-2 grid-rows-1',
  3: 'grid-cols-3 grid-rows-1',
  4: 'grid-cols-2 grid-rows-2',
};

/** Threads side by side, filling the window; empty slots offer a new thread. */
export function AgentGrid({
  sessions,
  panes,
  activeId,
  overId,
  onActivate,
  onOpenThread,
  onNewThread,
}: {
  /** the threads to show, at most `panes` of them */
  sessions: Session[];
  panes: 2 | 3 | 4;
  /** the thread keyboard shortcuts act on */
  activeId: string | null;
  overId: string | null;
  onActivate: (id: string) => void;
  onOpenThread: (id: string) => void;
  onNewThread: () => void;
}) {
  return (
    <div className="min-h-0 flex-1 px-4 pt-1 pb-4">
      <div className={cn('grid h-full gap-3', LAYOUT[panes])}>
        {sessions.map((s) => (
          <AgentTile
            key={s.id}
            session={s}
            active={activeId === s.id}
            dropActive={overId === s.id}
            onActivate={() => onActivate(s.id)}
            onOpen={() => onOpenThread(s.id)}
          />
        ))}
        {sessions.length < panes && (
        <button
          onClick={onNewThread}
          className="flex min-h-0 flex-col items-center justify-center gap-2 rounded-2xl border border-dashed border-border text-muted-foreground transition-colors hover:border-muted-foreground/40 hover:bg-accent/40 hover:text-foreground"
        >
          <Plus className="size-5" />
          <span className="text-[13px] font-medium">New thread</span>
        </button>
        )}
      </div>
    </div>
  );
}

function AgentTile({
  session,
  active,
  dropActive,
  onActivate,
  onOpen,
}: {
  session: Session;
  active: boolean;
  dropActive: boolean;
  onActivate: () => void;
  onOpen: () => void;
}) {
  const running = useAppStore((s) => !!s.running[session.id]);
  const streaming = useAppStore((s) => s.drafts[session.id] ?? '');
  const approvals = useAppStore((s) => s.approvals[session.id]);
  const bottomRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    bottomRef.current?.scrollIntoView({ block: 'end' });
  }, [session.messages.length, running, streaming, approvals?.length]);

  const last = session.messages[session.messages.length - 1];
  const needsReply = !running && last?.role === 'assistant';
  const recent = session.messages.slice(-TILE_TRANSCRIPT_SIZE);
  const folderLabel = session.runner === 'cloud' ? null : session.cwd ? basename(session.cwd) : 'Thread folder';

  return (
    <div
      data-session-id={session.id}
      // Clicking or typing into a tile makes it the one ⌘↵ enlarges.
      onFocusCapture={active ? undefined : onActivate}
      onMouseDown={active ? undefined : onActivate}
      className={cn(
        'relative flex min-h-0 min-w-0 flex-col overflow-hidden rounded-2xl border bg-card shadow-[0_1px_3px_rgba(0,0,0,0.04)] transition-shadow hover:shadow-[0_4px_16px_rgba(0,0,0,0.07)]',
        active ? 'border-foreground/40 ring-2 ring-foreground/[0.06]' : 'border-border',
      )}
    >
      <button
        onClick={onOpen}
        title="Open (⌘↵)"
        className="flex shrink-0 flex-col gap-0.5 border-b border-border px-3.5 py-2.5 text-left transition-colors hover:bg-accent/50"
      >
        <span className="flex min-w-0 items-center gap-2">
          <RunnerBadge runner={session.runner} className="size-4 shrink-0" />
          <span className="min-w-0 flex-1 truncate text-[13px] font-medium text-foreground">{session.title}</span>
          {approvals?.length ? (
            <span className="shrink-0 rounded-full bg-amber-500/10 px-1.5 py-0.5 text-[10px] font-medium text-amber-600">
              Needs approval
            </span>
          ) : running ? (
            <span className="size-1.5 shrink-0 animate-pulse rounded-full bg-chart-2" />
          ) : needsReply ? (
            <span className="shrink-0 rounded-full bg-status-success-background px-1.5 py-0.5 text-[10px] font-medium text-status-success">
              Needs reply
            </span>
          ) : null}
        </span>
        {folderLabel && <span className="truncate pl-6 text-[10.5px] text-muted-foreground-subtle">{folderLabel}</span>}
      </button>

      <div className="min-h-0 flex-1 overflow-y-auto scrollbar-sleek px-3.5 py-2.5">
        <div className="flex flex-col gap-2.5">
          {recent.length === 0 && <p className="text-[12px] text-muted-foreground-subtle italic">Nothing here yet</p>}
          <ThreadFolder session={session}>
            {groupMessages(recent).map((g) =>
              Array.isArray(g) ? <ToolGroup key={g[0].id} tools={g} dense /> : <MessageRow key={g.id} message={g} dense />,
            )}
            {streaming && <DraftRow text={streaming} dense />}
          </ThreadFolder>
          {approvals?.map((a) => <ApprovalCard key={a.id} approval={a} dense />)}
          <div ref={bottomRef} />
        </div>
      </div>

      <div className="shrink-0 border-t border-border p-2">
        <AttachmentChips session={session} />
        <TileComposer session={session} />
      </div>

      {dropActive && <DropOverlay />}
    </div>
  );
}

function TileComposer({ session }: { session: Session }) {
  const running = useAppStore((s) => !!s.running[session.id]);
  const send = useAppStore((s) => s.send);
  const stop = useAppStore((s) => s.stop);
  const [draft, setDraft] = useState('');
  const canStop = running && session.runner !== 'cloud';

  const submit = () => {
    const text = draft.trim();
    if (!text || running) return;
    setDraft('');
    void send(session.id, text);
  };

  return (
    <div
      className={cn(
        'flex items-end gap-1.5 rounded-xl border border-border bg-background px-2.5 py-1.5 transition-colors focus-within:border-muted-foreground/30',
        running && 'opacity-70',
      )}
    >
      <textarea
        value={draft}
        onChange={(e) => setDraft(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === 'Enter' && !e.shiftKey && !e.metaKey && !e.ctrlKey && !e.nativeEvent.isComposing) {
            e.preventDefault();
            submit();
          }
        }}
        rows={1}
        disabled={running}
        placeholder={running ? 'Working…' : 'Reply…'}
        className="max-h-20 min-h-6 w-full resize-none bg-transparent py-0.5 text-[12.5px] outline-none field-sizing-content placeholder:text-muted-foreground disabled:cursor-not-allowed"
      />
      {canStop ? (
        <button
          onClick={() => void stop(session.id)}
          aria-label="Stop"
          className="mb-0.5 flex size-6 shrink-0 items-center justify-center rounded-full bg-primary text-primary-foreground"
        >
          <Square className="size-2.5 fill-current" />
        </button>
      ) : (
        <button
          onClick={submit}
          disabled={!draft.trim() || running}
          aria-label="Send"
          className="mb-0.5 flex size-6 shrink-0 items-center justify-center rounded-full bg-primary text-primary-foreground transition-opacity disabled:bg-[#d9d9d9] disabled:opacity-100"
        >
          <ArrowUp className="size-3.5" />
        </button>
      )}
    </div>
  );
}
