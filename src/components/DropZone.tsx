import { useEffect, useRef, useState } from 'react';
import { Paperclip } from 'lucide-react';
import { useAppStore } from '@/store';

/**
 * Wires the Tauri webview's native drag & drop exactly once, and reports
 * which thread (if any) the pointer is currently dragging files over: the
 * whole thread in Focus, or whichever tile is under the cursor in Grid
 * (hit-tested via `data-session-id`). Local runners only. A plain `vite`
 * browser has no Tauri webview, so this is a silent no-op there.
 */
export function useDragDropTarget(activeSessionId: string | null, grid: boolean): string | null {
  const attachFiles = useAppStore((s) => s.attachFiles);
  const [overId, setOverId] = useState<string | null>(null);
  const activeRef = useRef(activeSessionId);
  const gridRef = useRef(grid);
  activeRef.current = activeSessionId;
  gridRef.current = grid;

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let cancelled = false;

    const localTarget = (id: string | null) => {
      if (!id) return null;
      const session = useAppStore.getState().sessions.find((s) => s.id === id);
      return session && session.runner !== 'cloud' ? id : null;
    };

    const hitTest = (pos: { x: number; y: number }) => {
      const ratio = window.devicePixelRatio || 1;
      const el = document.elementFromPoint(pos.x / ratio, pos.y / ratio);
      return (el as HTMLElement | null)?.closest<HTMLElement>('[data-session-id]')?.dataset.sessionId ?? null;
    };

    const targetFor = (pos: { x: number; y: number }) => localTarget(gridRef.current ? hitTest(pos) : activeRef.current);

    void (async () => {
      try {
        const { getCurrentWebview } = await import('@tauri-apps/api/webview');
        const off = await getCurrentWebview().onDragDropEvent((event) => {
          const p = event.payload;
          if (p.type === 'over') setOverId(targetFor(p.position));
          else if (p.type === 'drop') {
            const target = targetFor(p.position);
            if (target) void attachFiles(target, p.paths);
            setOverId(null);
          } else if (p.type === 'leave') setOverId(null);
        });
        if (cancelled) off();
        else unlisten = off;
      } catch {
        // Not running inside Tauri (plain `vite` in a browser) — no drag & drop.
      }
    })();

    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [attachFiles]);

  return overId;
}

/** Overlay shown on the thread currently under a native file drag. */
export function DropOverlay() {
  return (
    <div className="pointer-events-none absolute inset-0 z-20 flex animate-in items-center justify-center rounded-[inherit] border-2 border-dashed border-foreground/30 bg-background/80 fade-in-0 duration-150 backdrop-blur-[1px]">
      <div className="flex items-center gap-2 rounded-full bg-foreground px-3.5 py-1.5 text-[12.5px] font-medium text-background shadow-lg">
        <Paperclip className="size-3.5" />
        Drop to attach
      </div>
    </div>
  );
}
