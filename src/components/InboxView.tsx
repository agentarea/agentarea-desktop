import { useMemo, useState } from 'react';
import { Check, X, Cloud, AlertTriangle, MessageSquare, HelpCircle, ShieldQuestion, CircleCheck } from 'lucide-react';
import { cn } from '@/lib/utils';
import { useAppStore } from '@/store';
import { localInbox } from '@/lib/inbox';
import type { CloudInboxItem } from '@/lib/api';
import { RunnerBadge } from '@/assets/icons';
import { ApprovalActions } from '@/components/MessageView';

const CLOUD_LABEL: Record<string, { label: string; icon: typeof Check; tone: string }> = {
  waiting_for_approval: { label: 'Needs approval', icon: ShieldQuestion, tone: 'text-amber-600 bg-amber-500/10' },
  waiting_for_input: { label: 'Needs input', icon: HelpCircle, tone: 'text-blue-600 bg-blue-500/10' },
  failed: { label: 'Failed', icon: AlertTriangle, tone: 'text-destructive bg-destructive/10' },
  completed: { label: 'Completed', icon: CircleCheck, tone: 'text-muted-foreground bg-secondary' },
};

/** One list for everything waiting on you: cloud tasks from AgentArea and local threads. */
export function InboxView({ onOpenThread }: { onOpenThread: (id: string) => void }) {
  const sessions = useAppStore((s) => s.sessions);
  const running = useAppStore((s) => s.running);
  const approvals = useAppStore((s) => s.approvals);
  const cloud = useAppStore((s) => s.cloudInbox);
  const connected = useAppStore((s) => s.connection === 'connected');
  const openCloudTask = useAppStore((s) => s.openCloudTask);
  const [showCompleted, setShowCompleted] = useState(false);

  const local = useMemo(() => localInbox(sessions, running, approvals), [sessions, running, approvals]);
  const cloudShown = cloud.filter((i) => showCompleted || i.status !== 'completed');
  const empty = local.length === 0 && cloudShown.length === 0;

  return (
    <div className="min-h-0 flex-1 overflow-y-auto scrollbar-sleek">
      <div className="mx-auto flex max-w-3xl flex-col gap-6 px-6 pt-2 pb-10">
        <div className="flex items-center justify-between">
          <p className="text-[13px] text-muted-foreground">
            {empty ? 'Nothing needs you right now.' : 'Everything waiting on you, local and in AgentArea.'}
          </p>
          <label className="flex cursor-pointer items-center gap-1.5 text-[12px] text-muted-foreground select-none">
            <input type="checkbox" checked={showCompleted} onChange={(e) => setShowCompleted(e.target.checked)} />
            Show completed cloud tasks
          </label>
        </div>

        {local.length > 0 && (
          <Section title="On this device" count={local.length}>
            {local.map(({ session, kind, preview, approval }) => (
              <Row
                key={approval ? `${session.id}:${approval.id}` : session.id}
                onClick={() => onOpenThread(session.id)}
                icon={<RunnerBadge runner={session.runner} className="size-4" />}
                title={session.title}
                subtitle={approval?.title}
                badge={
                  kind === 'approval' ? (
                    <Badge tone="text-amber-600 bg-amber-500/10" icon={ShieldQuestion} label="Needs approval" />
                  ) : kind === 'failed' ? (
                    <Badge tone="text-destructive bg-destructive/10" icon={AlertTriangle} label="Failed" />
                  ) : (
                    <Badge tone="text-status-success bg-status-success-background" icon={MessageSquare} label="Needs reply" />
                  )
                }
                preview={preview}
                actions={approval ? <ApprovalActions approval={approval} compact /> : null}
              />
            ))}
          </Section>
        )}

        <Section title="AgentArea" count={cloudShown.length} icon={<Cloud className="size-3.5" />}>
          {!connected && <p className="px-3 py-2 text-[12.5px] text-muted-foreground">Sign in to see your workspace’s tasks.</p>}
          {connected && cloudShown.length === 0 && (
            <p className="px-3 py-2 text-[12.5px] text-muted-foreground">No cloud tasks need you.</p>
          )}
          {cloudShown.map((item) => (
            <CloudRow
              key={item.taskId}
              item={item}
              onOpen={() => {
                openCloudTask(item);
                const id = useAppStore.getState().selectedId;
                if (id) onOpenThread(id);
              }}
            />
          ))}
        </Section>
      </div>
    </div>
  );
}

function CloudRow({ item, onOpen }: { item: CloudInboxItem; onOpen: () => void }) {
  const resolveCloud = useAppStore((s) => s.resolveCloud);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const meta = CLOUD_LABEL[item.status] ?? CLOUD_LABEL.completed;
  const decide = async (approved: boolean) => {
    setBusy(true);
    setError(null);
    try {
      await resolveCloud(item, approved);
    } catch (e) {
      setError(String(e));
      setBusy(false);
    }
  };

  return (
    <Row
      onClick={onOpen}
      icon={<Cloud className="size-4 text-muted-foreground" />}
      title={item.description || 'Cloud task'}
      subtitle={item.agentName ?? undefined}
      badge={<Badge tone={meta.tone} icon={meta.icon} label={meta.label} />}
      preview={
        item.status === 'waiting_for_approval' && item.escalationTool
          ? `Wants to run ${item.escalationTool}`
          : item.status === 'failed'
            ? (item.error ?? undefined)
            : undefined
      }
      actions={
        item.status === 'waiting_for_approval' && item.escalationId ? (
          <div className="flex items-center gap-1.5" onClick={(e) => e.stopPropagation()}>
            <button
              disabled={busy}
              onClick={() => void decide(true)}
              className="flex items-center gap-1 rounded-full bg-foreground px-3 py-1 text-[12px] font-medium text-background disabled:opacity-50"
            >
              <Check className="size-3.5" /> Approve
            </button>
            <button
              disabled={busy}
              onClick={() => void decide(false)}
              className="flex items-center gap-1 rounded-full border border-border px-3 py-1 text-[12px] text-foreground hover:bg-accent disabled:opacity-50"
            >
              <X className="size-3.5" /> Deny
            </button>
          </div>
        ) : null
      }
      error={error}
    />
  );
}

function Section({
  title,
  count,
  icon,
  children,
}: {
  title: string;
  count: number;
  icon?: React.ReactNode;
  children: React.ReactNode;
}) {
  return (
    <section>
      <h2 className="mb-1.5 flex items-center gap-1.5 px-1 text-[12px] font-medium text-muted-foreground">
        {icon}
        {title}
        {count > 0 && <span className="rounded-full bg-secondary px-1.5 text-[11px] text-foreground">{count}</span>}
      </h2>
      <div className="flex flex-col overflow-hidden rounded-2xl border border-border">{children}</div>
    </section>
  );
}

function Row({
  onClick,
  icon,
  title,
  subtitle,
  badge,
  preview,
  actions,
  error,
}: {
  onClick: () => void;
  icon: React.ReactNode;
  title: string;
  subtitle?: string;
  badge: React.ReactNode;
  preview?: string;
  actions?: React.ReactNode;
  error?: string | null;
}) {
  return (
    <div
      onClick={onClick}
      className="flex cursor-default flex-col gap-1 border-b border-border px-3.5 py-2.5 transition-colors last:border-b-0 hover:bg-accent/50"
    >
      <div className="flex min-w-0 items-center gap-2">
        {icon}
        <span className="min-w-0 flex-1 truncate text-[13.5px] font-medium">{title}</span>
        {subtitle && <span className="shrink-0 text-[12px] text-muted-foreground">{subtitle}</span>}
        {badge}
      </div>
      {(preview || actions) && (
        <div className="flex min-w-0 items-center gap-3 pl-6">
          {preview && <span className="min-w-0 flex-1 truncate text-[12.5px] text-muted-foreground">{preview}</span>}
          {!preview && <span className="flex-1" />}
          {actions}
        </div>
      )}
      {error && <span className="pl-6 text-[12px] text-destructive">{error}</span>}
    </div>
  );
}

function Badge({ tone, icon: Icon, label }: { tone: string; icon: typeof Check; label: string }) {
  return (
    <span className={cn('flex shrink-0 items-center gap-1 rounded-full px-2 py-0.5 text-[11px] font-medium', tone)}>
      <Icon className="size-3" />
      {label}
    </span>
  );
}
