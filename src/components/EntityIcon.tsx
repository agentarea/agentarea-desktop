import { useState } from 'react';
import { cn } from '@/lib/utils';

/** Two letters from a name: "GitHub" → "GH", "google drive" → "GD". */
function initials(name: string): string {
  const words = name.replace(/[-_.]+/g, ' ').trim().split(/\s+/).filter(Boolean);
  if (words.length > 1) return (words[0][0] + words[1][0]).toUpperCase();
  const w = words[0] ?? '?';
  const caps = w.match(/[A-Z]/g);
  return (caps && caps.length > 1 ? caps[0] + caps[1] : w.slice(0, 2)).toUpperCase();
}

function hue(seed: string): number {
  let h = 0;
  for (const c of seed) h = (h * 31 + c.charCodeAt(0)) % 360;
  return h;
}

/**
 * An entity's logo: tries each URL in `icons` in order (registry icon, then the
 * host's favicon), and falls back to coloured initials when none loads.
 */
export function EntityIcon({
  name,
  icons,
  seed,
  className,
}: {
  name: string;
  icons?: string[];
  /** keeps the fallback colour stable per entity */
  seed?: string;
  className?: string;
}) {
  const [failed, setFailed] = useState(0);
  const src = icons?.[failed];
  return (
    <span
      className={cn(
        'flex size-5 shrink-0 items-center justify-center overflow-hidden rounded-[6px] text-[8.5px] font-semibold text-white',
        className,
      )}
      style={src ? { background: 'white', boxShadow: 'inset 0 0 0 1px rgba(0,0,0,0.08)' } : { backgroundColor: `hsl(${hue(seed ?? name)} 35% 50%)` }}
    >
      {src ? (
        <img src={src} alt="" className="size-[75%] object-contain" onError={() => setFailed((n) => n + 1)} />
      ) : (
        initials(name)
      )}
    </span>
  );
}
