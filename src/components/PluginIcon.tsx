import {
  Bell,
  BookMarked,
  BookOpen,
  Bot,
  Brain,
  Calendar,
  ChartColumn,
  ChartLine,
  ClipboardList,
  Cloud,
  Code,
  Cpu,
  Database,
  FileText,
  Folder,
  GitBranch,
  Globe,
  Image,
  Inbox,
  Kanban,
  LayoutDashboard,
  Library,
  ListTodo,
  Mail,
  Map,
  MessageSquare,
  Network,
  Newspaper,
  NotebookPen,
  Puzzle,
  Radio,
  RadioTower,
  Rss,
  Search,
  Server,
  Settings,
  ShoppingCart,
  Sparkles,
  SquareKanban,
  Table,
  Target,
  Terminal,
  Users,
  Wallet,
  Zap,
  type LucideIcon,
} from 'lucide-react';
import type { CSSProperties } from 'react';
import { cn } from '@/lib/utils';
import type { Plugin } from '@/lib/plugins';
import { EntityIcon } from './EntityIcon';

/** The lucide names a plugin.json `icon` may use. */
const LUCIDE: Record<string, LucideIcon> = {
  bell: Bell,
  'book-marked': BookMarked,
  'book-open': BookOpen,
  bot: Bot,
  brain: Brain,
  calendar: Calendar,
  'chart-column': ChartColumn,
  'chart-line': ChartLine,
  'clipboard-list': ClipboardList,
  cloud: Cloud,
  code: Code,
  cpu: Cpu,
  database: Database,
  'file-text': FileText,
  folder: Folder,
  'git-branch': GitBranch,
  globe: Globe,
  image: Image,
  inbox: Inbox,
  kanban: Kanban,
  'layout-dashboard': LayoutDashboard,
  library: Library,
  'list-todo': ListTodo,
  mail: Mail,
  map: Map,
  'message-square': MessageSquare,
  network: Network,
  newspaper: Newspaper,
  'notebook-pen': NotebookPen,
  puzzle: Puzzle,
  radio: Radio,
  'radio-tower': RadioTower,
  rss: Rss,
  search: Search,
  server: Server,
  settings: Settings,
  'shopping-cart': ShoppingCart,
  sparkles: Sparkles,
  'square-kanban': SquareKanban,
  table: Table,
  target: Target,
  terminal: Terminal,
  users: Users,
  wallet: Wallet,
  zap: Zap,
};

/**
 * A plugin's glyph in the app's own style: a lucide line icon, or the plugin's
 * SVG as a CSS mask filled with currentColor (so it follows the theme and never
 * runs as a document). Without either, the coloured initials badge.
 * `className` sizes and colours it like any lucide icon.
 */
export function PluginIcon({ plugin, className }: { plugin: Pick<Plugin, 'id' | 'title' | 'icon'>; className?: string }) {
  const icon = plugin.icon;
  if (icon?.startsWith('data:image/svg+xml')) {
    const mask = `url("${icon}") center / contain no-repeat`;
    const style: CSSProperties = { mask, WebkitMask: mask };
    return <span aria-hidden className={cn('inline-block size-4 shrink-0 bg-current', className)} style={style} />;
  }
  const Icon = icon ? LUCIDE[icon] : undefined;
  if (Icon) return <Icon aria-hidden className={cn('size-4 shrink-0', className)} />;
  return <EntityIcon name={plugin.title} seed={plugin.id} className={cn(className, 'rounded-[4px] text-[6.5px] text-white')} />;
}
