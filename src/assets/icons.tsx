import type { SVGProps } from 'react';
import { Cloud } from 'lucide-react';
import type { Runner } from '@/data';

export const RUNNER_LABEL = { claude: 'Claude', codex: 'Codex', cloud: 'Cloud' } as const;

/**
 * Simplified Claude "spark" mark: an eight-point sunburst radiating from a
 * shared center, in the brand's terracotta. Redrawn from memory in the
 * spirit of Simple Icons' `claude` glyph (CC0), not copied path data.
 */
export function ClaudeIcon({ className, ...props }: SVGProps<SVGSVGElement>) {
  const spokes = [0, 45, 90, 135, 180, 225, 270, 315];
  return (
    <svg viewBox="0 0 24 24" className={className} aria-hidden {...props}>
      <g fill="#D97757">
        {spokes.map((deg) => (
          <path key={deg} d="M12 12 C10.6 9.2 10.1 5.7 12 2 C13.9 5.7 13.4 9.2 12 12 Z" transform={`rotate(${deg} 12 12)`} />
        ))}
      </g>
    </svg>
  );
}

/**
 * Simplified OpenAI/Codex "knot" mark: six rounded lobes woven around an
 * open center. Redrawn from memory in the spirit of Simple Icons' `openai`
 * glyph (CC0), not copied path data. Uses currentColor.
 */
export function CodexIcon({ className, ...props }: SVGProps<SVGSVGElement>) {
  const spokes = [0, 60, 120, 180, 240, 300];
  return (
    <svg viewBox="0 0 24 24" className={className} aria-hidden {...props}>
      <g fill="currentColor">
        {spokes.map((deg) => (
          <rect key={deg} x="10.6" y="2.6" width="2.8" height="7.4" rx="1.4" transform={`rotate(${deg} 12 12)`} />
        ))}
      </g>
    </svg>
  );
}

/** Runner mark for chips, pickers and thread rows: brand logo for local CLIs, cloud icon for delegated agents. */
export function RunnerBadge({ runner, className }: { runner: Runner; className?: string }) {
  if (runner === 'claude') return <ClaudeIcon className={className} />;
  if (runner === 'codex') return <CodexIcon className={className} />;
  return <Cloud className={className} />;
}
