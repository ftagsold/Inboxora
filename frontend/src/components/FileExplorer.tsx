import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import type { CSSProperties, DragEvent as ReactDragEvent, KeyboardEvent as ReactKeyboardEvent, MouseEvent as ReactMouseEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { api } from '../utils/api.ts';
import type { WebDavColumn, WebDavEntry, WebDavServer, WebDavView } from '../utils/api.ts';
import { toAppError } from '../utils/errors.ts';
import { Button, Dialog, EmptyState, inputStyle } from './ui.tsx';
import { EditIcon, TrashIcon } from './menuIcons.tsx';
import { FileIcon, extensionOf, fileKind, previewable } from './fileIcons.tsx';
import { useUiScale } from '../hooks/useUiScale.ts';
import './files.css';

/** What the explorer is for: managing files, or choosing files to attach. */
export type ExplorerMode = 'browse' | 'pick';

interface FileExplorerProps {
  server: WebDavServer;
  mode?: ExplorerMode;
  /** Pick mode: the files currently chosen (folders are only navigated). */
  onPickChange?: (entries: WebDavEntry[]) => void;
  /** Pick mode: bytes still allowed in total. Files that would push the chosen set past it are
   * shown disabled; a file of unknown size is allowed. */
  pickBudget?: number;
}

type SortKey = 'name' | 'type' | 'size' | 'modified' | 'location';
interface Clipboard { op: 'copy' | 'cut'; serverId: string; entries: WebDavEntry[] }
interface Upload { id: number; name: string; progress: number; error?: string; done?: boolean }
interface MenuState { x: number; y: number; target: WebDavEntry | null; kind?: 'columns' }
type NameDialog = { kind: 'mkdir' } | { kind: 'rename'; entry: WebDavEntry };

const VIEW_KEY = 'inboxora_files_view';
const SORT_KEY = 'inboxora_files_sort';
const DRAG_TYPE = 'application/x-inboxora-webdav';
// One clipboard for the whole app, so cut/copy survives switching folders (same server only).
let sharedClipboard: Clipboard | null = null;
let uploadSeq = 0;

// ── column layout (stored per user in the backend) ───────────────────────────
const COLUMN_ORDER: WebDavColumn[] = ['location', 'type', 'size', 'modified'];
const DEFAULT_WIDTHS: Record<WebDavColumn, number> = { location: 220, type: 110, size: 96, modified: 170 };
const DEFAULT_VIEW: WebDavView = { columns: ['size', 'modified', 'location'], widths: {} };
let viewCache: WebDavView | null = null;
let viewRequest: Promise<WebDavView> | null = null;
let viewSaveTimer: ReturnType<typeof setTimeout> | null = null;
const viewListeners = new Set<(view: WebDavView) => void>();

function loadView(): Promise<WebDavView> {
  if (viewCache) return Promise.resolve(viewCache);
  viewRequest ??= api.webdav.getView().then(result => { viewCache = result.view; return result.view; }).catch(() => { viewRequest = null; return DEFAULT_VIEW; });
  return viewRequest;
}

/** Apply a layout change everywhere at once and save it shortly after. */
function updateView(next: WebDavView, persist = true) {
  viewCache = next;
  viewListeners.forEach(listener => listener(next));
  if (!persist) return;
  if (viewSaveTimer) clearTimeout(viewSaveTimer);
  viewSaveTimer = setTimeout(() => { api.webdav.saveView(next).catch(() => {}); }, 500);
}

function useExplorerView(): WebDavView {
  const [view, setView] = useState<WebDavView>(viewCache ?? DEFAULT_VIEW);
  useEffect(() => {
    viewListeners.add(setView);
    void loadView().then(setView);
    return () => { viewListeners.delete(setView); };
  }, []);
  return view;
}

/** What the type column shows: "Folder", or the extension in capitals. */
function typeLabel(entry: WebDavEntry, folder: string): string {
  if (entry.isDir) return folder;
  const ext = extensionOf(entry.name);
  return ext ? ext.toUpperCase() : (entry.contentType?.split('/')[1]?.toUpperCase() ?? '');
}
const MOD_KEY = typeof navigator !== 'undefined' && /mac|iphone|ipad/i.test(navigator.platform || navigator.userAgent) ? '⌘' : 'Ctrl+';

function readStored<T>(key: string, fallback: T): T {
  try { const raw = localStorage.getItem(key); return raw ? JSON.parse(raw) as T : fallback; } catch { return fallback; }
}
function writeStored(key: string, value: unknown) {
  try { localStorage.setItem(key, JSON.stringify(value)); } catch { /* storage unavailable */ }
}

/** `dir` (ending with "/") joined with one child name. */
export function childPath(dir: string, name: string, isDir = false): string {
  const base = dir.endsWith('/') ? dir : `${dir}/`;
  return `${base}${name}${isDir ? '/' : ''}`;
}

/** The parent folder of a path ("/" for top-level items). */
export function parentPath(path: string): string {
  const trimmed = path.replace(/\/+$/, '');
  const cut = trimmed.lastIndexOf('/');
  return cut <= 0 ? '/' : `${trimmed.slice(0, cut)}/`;
}

/** Breadcrumb segments of a folder path: [{ name, path }]. */
export function crumbs(path: string): Array<{ name: string; path: string }> {
  const parts = path.split('/').filter(Boolean);
  return parts.map((name, index) => ({ name, path: `/${parts.slice(0, index + 1).join('/')}/` }));
}

export function formatSize(bytes: number | null | undefined): string {
  if (bytes == null) return '';
  if (bytes < 1024) return `${bytes} B`;
  const units = ['KB', 'MB', 'GB', 'TB'];
  let value = bytes / 1024; let unit = 0;
  while (value >= 1024 && unit < units.length - 1) { value /= 1024; unit += 1; }
  return `${value < 10 ? value.toFixed(1) : Math.round(value)} ${units[unit]}`;
}

function invalidName(name: string): boolean {
  return !name.trim() || name.includes('/') || name.includes('\\') || name === '.' || name === '..';
}

/** A name not already taken in `taken`, adding " (2)", " (3)" … before the extension. */
function freeName(name: string, taken: Set<string>): string {
  if (!taken.has(name)) return name;
  const dot = name.lastIndexOf('.');
  const [stem, ext] = dot > 0 ? [name.slice(0, dot), name.slice(dot)] : [name, ''];
  for (let n = 2; ; n += 1) { const candidate = `${stem} (${n})${ext}`; if (!taken.has(candidate)) return candidate; }
}

export default function FileExplorer({ server, mode = 'browse', onPickChange, pickBudget = Infinity }: FileExplorerProps) {
  const { t, i18n } = useTranslation();
  const picking = mode === 'pick';
  const [path, setPath] = useState('/');
  const [entries, setEntries] = useState<WebDavEntry[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [picked, setPicked] = useState<Map<string, WebDavEntry>>(new Map());
  const [anchor, setAnchor] = useState<string | null>(null);
  const [filter, setFilter] = useState('');
  const [view, setView] = useState<'list' | 'grid'>(() => readStored(VIEW_KEY, 'list'));
  const [sort, setSort] = useState<{ key: SortKey; desc: boolean }>(() => readStored(SORT_KEY, { key: 'name', desc: false }));
  const [menu, setMenu] = useState<MenuState | null>(null);
  const [nameDialog, setNameDialog] = useState<NameDialog | null>(null);
  const [nameValue, setNameValue] = useState('');
  const [confirmDelete, setConfirmDelete] = useState<WebDavEntry[] | null>(null);
  const [preview, setPreview] = useState<WebDavEntry | null>(null);
  const [busy, setBusy] = useState(false);
  const [uploads, setUploads] = useState<Upload[]>([]);
  const [dropTarget, setDropTarget] = useState<string | null>(null);
  const [clipboard, setClipboardState] = useState<Clipboard | null>(sharedClipboard);
  const [searchResult, setSearchResult] = useState<{ query: string; entries: WebDavEntry[]; truncated: boolean } | null>(null);
  const [searching, setSearching] = useState(false);
  const [searchRun, setSearchRun] = useState(0);
  const layout = useExplorerView();
  const scale = useUiScale();
  const listRef = useRef<HTMLDivElement | null>(null);
  const fileInputRef = useRef<HTMLInputElement | null>(null);
  const loadSeq = useRef(0);
  const dragDepth = useRef(0);

  const setClipboard = (next: Clipboard | null) => { sharedClipboard = next; setClipboardState(next); };

  const load = useCallback(async (target: string, { keepSelection = false } = {}) => {
    const seq = ++loadSeq.current;
    setLoading(true); setError(null);
    try {
      const result = await api.webdav.list(server.id, target);
      if (seq !== loadSeq.current) return;
      setEntries(result.entries);
      setPath(result.path || target);
      if (!keepSelection) { setSelected(new Set()); setAnchor(null); }
    } catch (err) {
      if (seq !== loadSeq.current) return;
      setError(toAppError(err).message);
      setEntries([]);
    } finally {
      if (seq === loadSeq.current) setLoading(false);
    }
  }, [server.id]);

  // A different server starts at its root with a clean slate.
  useEffect(() => {
    setFilter(''); setPicked(new Map()); setPreview(null); setMenu(null);
    void load('/');
  }, [load]);

  useEffect(() => { onPickChange?.([...picked.values()]); }, [picked, onPickChange]);
  useEffect(() => { if (!notice) return; const timer = setTimeout(() => setNotice(null), 3500); return () => clearTimeout(timer); }, [notice]);

  const navigate = (target: string) => { setFilter(''); void load(target); listRef.current?.focus(); };
  const query = filter.trim();
  const refresh = () => { if (query) setSearchRun(run => run + 1); return load(path, { keepSelection: true }); };

  // Deep search: the server walks every folder below the current one. Until it answers, the
  // current folder is filtered locally so typing still shows something at once.
  useEffect(() => {
    if (!query) { setSearchResult(null); setSearching(false); return; }
    const controller = new AbortController();
    setSearching(true);
    const timer = setTimeout(() => {
      api.webdav.search(server.id, path, query, controller.signal)
        .then(result => setSearchResult({ query, entries: result.entries, truncated: result.truncated }))
        .catch(err => { if (!controller.signal.aborted) setError(toAppError(err).message); })
        .finally(() => { if (!controller.signal.aborted) setSearching(false); });
    }, 350);
    return () => { clearTimeout(timer); controller.abort(); };
  }, [query, path, server.id, searchRun]);
  const searchActive = Boolean(query && searchResult?.query === query);

  const visible = useMemo(() => {
    const needle = query.toLowerCase();
    const list = searchActive && searchResult ? [...searchResult.entries]
      : needle ? entries.filter(entry => entry.name.toLowerCase().includes(needle)) : [...entries];
    const direction = sort.desc ? -1 : 1;
    const text = (entry: WebDavEntry) => sort.key === 'type' ? typeLabel(entry, '') : sort.key === 'location' ? parentPath(entry.path) : entry.name;
    list.sort((a, b) => {
      if (a.isDir !== b.isDir) return a.isDir ? -1 : 1;
      let diff = 0;
      if (sort.key === 'size') diff = (a.size ?? 0) - (b.size ?? 0);
      else if (sort.key === 'modified') diff = (a.modified ? Date.parse(a.modified) : 0) - (b.modified ? Date.parse(b.modified) : 0);
      else if (sort.key !== 'name') diff = text(a).localeCompare(text(b), undefined, { numeric: true, sensitivity: 'base' });
      if (diff === 0) diff = a.name.localeCompare(b.name, undefined, { numeric: true, sensitivity: 'base' });
      return diff * direction;
    });
    return list;
  }, [entries, query, searchActive, searchResult, sort]);

  // Columns shown after the name; "location" only means something in search results.
  const columns = COLUMN_ORDER.filter(column => layout.columns.includes(column) && (column !== 'location' || searchActive));
  const [liveWidths, setLiveWidths] = useState<WebDavView['widths'] | null>(null);
  const widths = liveWidths ?? layout.widths;
  const gridTemplate = [
    picking ? '22px' : null,
    widths.name ? `${widths.name}px` : 'minmax(160px, 1fr)',
    ...columns.map(column => `${widths[column] ?? DEFAULT_WIDTHS[column]}px`),
    widths.name ? 'minmax(0, 1fr)' : null,
  ].filter(Boolean).join(' ');

  const startResize = (event: ReactMouseEvent, column: 'name' | WebDavColumn) => {
    event.preventDefault(); event.stopPropagation();
    const cell = (event.currentTarget as HTMLElement).parentElement;
    const startX = event.clientX;
    const startWidth = (cell?.getBoundingClientRect().width ?? 100) / scale;
    let latest = { ...widths };
    const move = (moveEvent: MouseEvent) => {
      latest = { ...widths, [column]: Math.round(Math.max(48, Math.min(1200, startWidth + (moveEvent.clientX - startX) / scale))) };
      setLiveWidths(latest);
    };
    const up = () => {
      document.removeEventListener('mousemove', move);
      document.removeEventListener('mouseup', up);
      document.body.classList.remove('files-resizing');
      setLiveWidths(null);
      updateView({ ...layout, widths: latest });
    };
    document.body.classList.add('files-resizing');
    document.addEventListener('mousemove', move);
    document.addEventListener('mouseup', up);
  };
  const toggleColumn = (column: WebDavColumn) => {
    const next = layout.columns.includes(column) ? layout.columns.filter(item => item !== column) : [...layout.columns, column];
    updateView({ ...layout, columns: COLUMN_ORDER.filter(item => next.includes(item)) });
  };

  const selectedEntries = useMemo(() => visible.filter(entry => selected.has(entry.path)), [visible, selected]);
  const dateFormat = useMemo(() => new Intl.DateTimeFormat(i18n.resolvedLanguage || i18n.language || undefined, { dateStyle: 'medium', timeStyle: 'short' }), [i18n.resolvedLanguage, i18n.language]);

  const changeSort = (key: SortKey) => setSort(current => {
    const next = { key, desc: current.key === key ? !current.desc : false };
    writeStored(SORT_KEY, next);
    return next;
  });
  const changeView = (next: 'list' | 'grid') => { setView(next); writeStored(VIEW_KEY, next); };

  // ── selection ──────────────────────────────────────────────────────────────
  const selectClick = (entry: WebDavEntry, event: ReactMouseEvent) => {
    if (event.shiftKey && anchor) {
      const from = visible.findIndex(item => item.path === anchor);
      const to = visible.findIndex(item => item.path === entry.path);
      if (from >= 0 && to >= 0) {
        const [lo, hi] = from < to ? [from, to] : [to, from];
        setSelected(new Set(visible.slice(lo, hi + 1).map(item => item.path)));
        return;
      }
    }
    if (event.metaKey || event.ctrlKey) {
      setSelected(current => { const next = new Set(current); if (next.has(entry.path)) next.delete(entry.path); else next.add(entry.path); return next; });
    } else {
      setSelected(new Set([entry.path]));
    }
    setAnchor(entry.path);
  };

  const pickedBytes = [...picked.values()].reduce((sum, entry) => sum + (entry.size ?? 0), 0);
  // Re-evaluated after every pick: what was choosable may stop fitting once others are chosen.
  const tooLarge = (entry: WebDavEntry) => picking && !entry.isDir && !picked.has(entry.path) && pickedBytes + (entry.size ?? 0) > pickBudget;
  const togglePick = (entry: WebDavEntry) => setPicked(current => {
    const next = new Map(current);
    if (next.has(entry.path)) { next.delete(entry.path); return next; }
    const used = [...current.values()].reduce((sum, item) => sum + (item.size ?? 0), 0);
    if (used + (entry.size ?? 0) > pickBudget) return current;
    next.set(entry.path, entry);
    return next;
  });

  const open = (entry: WebDavEntry) => {
    if (entry.isDir) { navigate(entry.path); return; }
    if (picking) { togglePick(entry); return; }
    if (previewable(entry)) setPreview(entry); else download([entry]);
  };

  // ── actions ────────────────────────────────────────────────────────────────
  const download = (items: WebDavEntry[]) => {
    for (const entry of items.filter(item => !item.isDir)) {
      const link = document.createElement('a');
      link.href = api.webdav.fileUrl(server.id, entry.path);
      link.download = entry.name;
      link.rel = 'noopener';
      document.body.appendChild(link); link.click(); link.remove();
    }
  };

  const run = async (work: () => Promise<unknown>, success?: string) => {
    setBusy(true); setError(null);
    try { await work(); if (success) setNotice(success); } catch (err) { setError(toAppError(err).message); } finally { setBusy(false); await refresh(); }
  };

  const openNameDialog = (dialog: NameDialog) => {
    setMenu(null);
    setNameValue(dialog.kind === 'rename' ? dialog.entry.name : t('files.newFolderName'));
    setNameDialog(dialog);
  };

  const submitName = async () => {
    if (!nameDialog || invalidName(nameValue)) return;
    const name = nameValue.trim();
    const dialog = nameDialog;
    setNameDialog(null);
    if (dialog.kind === 'mkdir') {
      await run(() => api.webdav.mkdir(server.id, childPath(path, name, true)), t('files.folderCreated'));
    } else if (name !== dialog.entry.name) {
      const target = childPath(parentPath(dialog.entry.path), name, dialog.entry.isDir);
      await run(() => api.webdav.move(server.id, dialog.entry.path, target, dialog.entry.isDir), t('files.renamed'));
      setSelected(new Set([target]));
    }
  };

  const remove = async (items: WebDavEntry[]) => {
    setConfirmDelete(null);
    await run(async () => {
      for (const entry of items) await api.webdav.remove(server.id, entry.path, entry.isDir);
    }, t('files.deleted', { count: items.length }));
  };

  const copyOrCut = (op: Clipboard['op'], items: WebDavEntry[]) => {
    if (!items.length) return;
    setClipboard({ op, serverId: server.id, entries: items });
    setNotice(t(op === 'copy' ? 'files.copiedToClipboard' : 'files.cutToClipboard', { count: items.length }));
  };

  const canPaste = Boolean(clipboard && clipboard.serverId === server.id && clipboard.entries.length);

  const paste = async (targetDir: string) => {
    const board = clipboard;
    if (!board || board.serverId !== server.id) return;
    const taken = new Set(entries.map(entry => entry.name));
    await run(async () => {
      for (const entry of board.entries) {
        if (board.op === 'cut' && parentPath(entry.path) === targetDir) continue;
        if (entry.isDir && targetDir.startsWith(entry.path)) throw new Error(t('files.intoItself'));
        const name = targetDir === path ? freeName(entry.name, taken) : entry.name;
        taken.add(name);
        const target = childPath(targetDir, name, entry.isDir);
        if (board.op === 'copy') await api.webdav.copy(server.id, entry.path, target, entry.isDir);
        else await api.webdav.move(server.id, entry.path, target, entry.isDir);
      }
      if (board.op === 'cut') setClipboard(null);
    }, t('files.pasted'));
  };

  const duplicate = async (entry: WebDavEntry) => {
    const name = freeName(entry.name, new Set(entries.map(item => item.name)));
    await run(() => api.webdav.copy(server.id, entry.path, childPath(parentPath(entry.path), name, entry.isDir), entry.isDir), t('files.duplicated'));
  };

  const moveInto = async (items: WebDavEntry[], targetDir: string) => {
    const movable = items.filter(entry => parentPath(entry.path) !== targetDir && !(entry.isDir && targetDir.startsWith(entry.path)));
    if (!movable.length) return;
    await run(async () => {
      for (const entry of movable) await api.webdav.move(server.id, entry.path, childPath(targetDir, entry.name, entry.isDir), entry.isDir);
    }, t('files.moved', { count: movable.length }));
  };

  const copyLink = async (entry: WebDavEntry) => {
    const root = server.url.replace(/\/+$/, '');
    const link = `${root}${entry.path.split('/').map(encodeURIComponent).join('/')}`;
    try { await navigator.clipboard.writeText(link); setNotice(t('files.linkCopied')); } catch { setError(t('files.clipboardFailed')); }
  };

  const uploadFiles = async (files: File[], targetDir = path) => {
    if (!files.length || picking) return;
    const jobs = files.map(file => ({ file, id: ++uploadSeq }));
    setUploads(current => [...current, ...jobs.map(job => ({ id: job.id, name: job.file.name, progress: 0 }))]);
    const update = (id: number, patch: Partial<Upload>) => setUploads(current => current.map(item => item.id === id ? { ...item, ...patch } : item));
    for (const { file, id } of jobs) {
      const target = childPath(targetDir, file.name);
      const send = (overwrite: boolean) => api.webdav.upload(server.id, target, file, { overwrite, onProgress: progress => update(id, { progress }) });
      try {
        try { await send(false); } catch (err) {
          if ((err as { status?: number }).status !== 409 && (err as { status?: number }).status !== 412) throw err;
          if (!window.confirm(t('files.overwriteConfirm', { name: file.name }))) { update(id, { done: true, error: t('files.skipped') }); continue; }
          await send(true);
        }
        update(id, { progress: 1, done: true });
      } catch (err) {
        update(id, { done: true, error: toAppError(err).message });
      }
    }
    await refresh();
    // Finished rows fade away on their own; failed ones stay until dismissed.
    setTimeout(() => setUploads(current => current.filter(item => !item.done || item.error)), 2500);
  };

  // ── context menu ───────────────────────────────────────────────────────────
  const openMenuAt = (x: number, y: number, target: WebDavEntry | null) => {
    if (target && !selected.has(target.path)) { setSelected(new Set([target.path])); setAnchor(target.path); }
    if (!target) setSelected(new Set());
    setMenu({ x, y, target });
  };
  const openMenu = (event: ReactMouseEvent, target: WebDavEntry | null) => {
    event.preventDefault(); event.stopPropagation();
    openMenuAt(event.clientX, event.clientY, target);
  };
  // iOS Safari fires no contextmenu event: a 500 ms press opens the same menu.
  const longPress = useRef<{ timer: ReturnType<typeof setTimeout>; fired: boolean } | null>(null);
  const cancelLongPress = () => { if (longPress.current) clearTimeout(longPress.current.timer); };
  const touchMenuProps = (entry: WebDavEntry) => ({
    onTouchStart: (event: React.TouchEvent) => {
      cancelLongPress();
      const { clientX, clientY } = event.touches[0];
      const state = { fired: false, timer: setTimeout(() => { state.fired = true; openMenuAt(clientX, clientY, entry); }, 500) };
      longPress.current = state;
    },
    onTouchMove: cancelLongPress,
    onTouchEnd: (event: React.TouchEvent) => { cancelLongPress(); if (longPress.current?.fired) event.preventDefault(); },
  });

  useEffect(() => {
    if (!menu) return;
    const close = (event: Event) => {
      if (event instanceof MouseEvent && event.target instanceof Element && event.target.closest('.files-menu, .files-columns-button')) return;
      setMenu(null);
    };
    const key = (event: KeyboardEvent) => { if (event.key === 'Escape') { event.stopPropagation(); setMenu(null); } };
    document.addEventListener('mousedown', close);
    document.addEventListener('scroll', close, true);
    window.addEventListener('resize', close);
    document.addEventListener('keydown', key, true);
    return () => {
      document.removeEventListener('mousedown', close);
      document.removeEventListener('scroll', close, true);
      window.removeEventListener('resize', close);
      document.removeEventListener('keydown', key, true);
    };
  }, [menu]);

  // ── keyboard ───────────────────────────────────────────────────────────────
  const onKeyDown = (event: ReactKeyboardEvent<HTMLDivElement>) => {
    if (event.target instanceof HTMLInputElement) return;
    const mod = event.metaKey || event.ctrlKey;
    const current = anchor ? visible.findIndex(item => item.path === anchor) : -1;
    const focusIndex = (index: number) => {
      const entry = visible[Math.max(0, Math.min(visible.length - 1, index))];
      if (!entry) return;
      setSelected(new Set([entry.path])); setAnchor(entry.path);
      listRef.current?.querySelector<HTMLElement>(`[data-path="${CSS.escape(entry.path)}"]`)?.scrollIntoView({ block: 'nearest' });
    };
    const step = view === 'grid' ? Math.max(1, Math.floor((listRef.current?.clientWidth ?? 600) / 132)) : 1;
    if (event.key === 'ArrowDown') { event.preventDefault(); focusIndex(current + step); }
    else if (event.key === 'ArrowUp') { event.preventDefault(); focusIndex(current < 0 ? 0 : current - step); }
    else if (view === 'grid' && event.key === 'ArrowRight') { event.preventDefault(); focusIndex(current + 1); }
    else if (view === 'grid' && event.key === 'ArrowLeft') { event.preventDefault(); focusIndex(current - 1); }
    else if (event.key === 'Enter' && selectedEntries.length === 1) { event.preventDefault(); open(selectedEntries[0]); }
    else if (event.key === ' ' && picking && selectedEntries.length === 1 && !selectedEntries[0].isDir) { event.preventDefault(); togglePick(selectedEntries[0]); }
    // Finder deletes with ⌘⌫; plain Backspace goes up a folder.
    else if (mod && event.key === 'Backspace' && !picking && selectedEntries.length) { event.preventDefault(); setConfirmDelete(selectedEntries); }
    else if (event.key === 'Backspace' && path !== '/') { event.preventDefault(); navigate(parentPath(path)); }
    else if (event.key === 'Escape') { setSelected(new Set()); }
    else if (mod && event.key.toLowerCase() === 'a') { event.preventDefault(); setSelected(new Set(visible.map(item => item.path))); }
    else if (picking) return;
    else if (event.key === 'Delete' && selectedEntries.length) { event.preventDefault(); setConfirmDelete(selectedEntries); }
    else if (event.key === 'F2' && selectedEntries.length === 1) { event.preventDefault(); openNameDialog({ kind: 'rename', entry: selectedEntries[0] }); }
    else if (mod && event.key.toLowerCase() === 'c') { event.preventDefault(); copyOrCut('copy', selectedEntries); }
    else if (mod && event.key.toLowerCase() === 'x') { event.preventDefault(); copyOrCut('cut', selectedEntries); }
    else if (mod && event.key.toLowerCase() === 'v' && canPaste) { event.preventDefault(); void paste(path); }
  };

  // ── drag and drop ──────────────────────────────────────────────────────────
  const draggedEntries = (event: ReactDragEvent): WebDavEntry[] | null => {
    const raw = event.dataTransfer.getData(DRAG_TYPE);
    if (!raw) return null;
    try { const data = JSON.parse(raw) as { serverId: string; entries: WebDavEntry[] }; return data.serverId === server.id ? data.entries : null; } catch { return null; }
  };
  const isExternalFiles = (event: ReactDragEvent) => [...event.dataTransfer.types].includes('Files');
  const isInternal = (event: ReactDragEvent) => [...event.dataTransfer.types].includes(DRAG_TYPE);

  const onRowDragStart = (event: ReactDragEvent, entry: WebDavEntry) => {
    const items = selected.has(entry.path) ? selectedEntries : [entry];
    if (!selected.has(entry.path)) { setSelected(new Set([entry.path])); setAnchor(entry.path); }
    event.dataTransfer.setData(DRAG_TYPE, JSON.stringify({ serverId: server.id, entries: items }));
    event.dataTransfer.effectAllowed = 'move';
  };

  const folderDropProps = (folder: string) => picking ? {} : {
    onDragOver: (event: ReactDragEvent) => {
      if (!isInternal(event) && !isExternalFiles(event)) return;
      event.preventDefault(); event.stopPropagation();
      event.dataTransfer.dropEffect = isInternal(event) ? 'move' : 'copy';
      setDropTarget(folder);
    },
    onDragLeave: (event: ReactDragEvent) => { event.stopPropagation(); setDropTarget(current => current === folder ? null : current); },
    onDrop: (event: ReactDragEvent) => {
      event.preventDefault(); event.stopPropagation();
      dragDepth.current = 0; setDropTarget(null);
      const internal = draggedEntries(event);
      if (internal) void moveInto(internal, folder);
      else if (event.dataTransfer.files.length) void uploadFiles([...event.dataTransfer.files], folder);
    },
  };

  const surfaceDrop = picking ? {} : {
    onDragEnter: (event: ReactDragEvent) => { if (isExternalFiles(event)) { dragDepth.current += 1; setDropTarget(current => current ?? path); } },
    onDragLeave: () => { dragDepth.current = Math.max(0, dragDepth.current - 1); if (!dragDepth.current) setDropTarget(null); },
    onDragOver: (event: ReactDragEvent) => { if (isExternalFiles(event)) { event.preventDefault(); event.dataTransfer.dropEffect = 'copy'; } },
    onDrop: (event: ReactDragEvent) => {
      event.preventDefault(); dragDepth.current = 0; setDropTarget(null);
      if (event.dataTransfer.files.length) void uploadFiles([...event.dataTransfer.files], path);
    },
  };

  // ── rendering ──────────────────────────────────────────────────────────────
  const single = menu?.target && selectedEntries.length <= 1 ? menu.target : null;
  const columnLabels: Record<WebDavColumn, string> = { location: t('files.location'), type: t('files.type'), size: t('files.size'), modified: t('files.modified') };
  const menuItems = menu ? (menu.kind === 'columns' ? [
    ...COLUMN_ORDER.map(column => ({ key: `col-${column}`, label: columnLabels[column], checked: layout.columns.includes(column), keepOpen: true, onClick: () => toggleColumn(column), show: true })),
    { key: 'sep-cols', separator: true, show: true },
    { key: 'reset', label: t('files.resetWidths'), icon: <RefreshIcon />, onClick: () => updateView({ ...layout, widths: {} }), show: Object.keys(layout.widths).length > 0 },
  ] : menu.target ? [
    { key: 'open', label: menu.target.isDir ? t('files.open') : (previewable(menu.target) ? t('files.preview') : t('files.download')), icon: <OpenIcon />, onClick: () => open(menu.target!), show: Boolean(single) },
    { key: 'download', label: selectedEntries.length > 1 ? t('files.downloadCount', { count: selectedEntries.filter(item => !item.isDir).length }) : t('files.download'), icon: <DownloadIcon />, onClick: () => download(selectedEntries), show: selectedEntries.some(item => !item.isDir) },
    { key: 'sep1', separator: true, show: !picking },
    { key: 'rename', label: t('files.rename'), icon: <EditIcon />, onClick: () => openNameDialog({ kind: 'rename', entry: menu.target! }), show: Boolean(single) && !picking, hint: 'F2' },
    { key: 'duplicate', label: t('files.duplicate'), icon: <CopyIcon />, onClick: () => duplicate(menu.target!), show: Boolean(single) && !picking },
    { key: 'copy', label: t('files.copy'), icon: <CopyIcon />, onClick: () => copyOrCut('copy', selectedEntries), show: !picking, hint: `${MOD_KEY}C` },
    { key: 'cut', label: t('files.cut'), icon: <CutIcon />, onClick: () => copyOrCut('cut', selectedEntries), show: !picking, hint: `${MOD_KEY}X` },
    { key: 'paste-into', label: t('files.pasteInto'), icon: <PasteIcon />, onClick: () => paste(menu.target!.path), show: Boolean(single?.isDir) && canPaste && !picking },
    { key: 'link', label: t('files.copyLink'), icon: <LinkIcon />, onClick: () => copyLink(menu.target!), show: Boolean(single) },
    { key: 'reveal', label: t('files.showInFolder'), icon: <FolderIcon />, onClick: () => navigate(parentPath(menu.target!.path)), show: Boolean(single) && searchActive && parentPath(menu.target.path) !== path },
    { key: 'sep2', separator: true, show: !picking },
    { key: 'delete', label: selectedEntries.length > 1 ? t('files.deleteCount', { count: selectedEntries.length }) : t('files.delete'), icon: <TrashIcon />, onClick: () => setConfirmDelete(selectedEntries.length ? selectedEntries : [menu.target!]), show: !picking, danger: true, hint: MOD_KEY === '⌘' ? '⌘⌫' : 'Del' },
  ] : [
    { key: 'mkdir', label: t('files.newFolder'), icon: <FolderPlusIcon />, onClick: () => openNameDialog({ kind: 'mkdir' }), show: !picking },
    { key: 'upload', label: t('files.upload'), icon: <UploadIcon />, onClick: () => fileInputRef.current?.click(), show: !picking },
    { key: 'paste', label: t('files.paste'), icon: <PasteIcon />, onClick: () => paste(path), show: canPaste && !picking, hint: `${MOD_KEY}V` },
    { key: 'sep', separator: true, show: !picking },
    { key: 'select-all', label: t('files.selectAll'), icon: <SelectIcon />, onClick: () => setSelected(new Set(visible.map(item => item.path))), show: visible.length > 0 },
    { key: 'refresh', label: t('files.refresh'), icon: <RefreshIcon />, onClick: () => refresh(), show: true },
  ]).filter(item => item.show) : [];

  const totalSelectedSize = selectedEntries.reduce((sum, entry) => sum + (entry.size ?? 0), 0);
  const cutPaths = clipboard?.op === 'cut' && clipboard.serverId === server.id ? new Set(clipboard.entries.map(entry => entry.path)) : null;

  const rowProps = (entry: WebDavEntry) => ({
    'data-path': entry.path,
    'data-testid': 'files-entry',
    role: 'row' as const,
    'aria-selected': selected.has(entry.path),
    'aria-disabled': tooLarge(entry) || undefined,
    title: tooLarge(entry) ? t('files.tooLargeToAttach', { size: formatSize(Math.max(0, pickBudget - pickedBytes)) }) : undefined,
    draggable: !picking,
    className: [
      view === 'grid' ? 'files-tile' : 'files-row',
      selected.has(entry.path) ? 'is-selected' : '',
      picked.has(entry.path) ? 'is-picked' : '',
      tooLarge(entry) ? 'is-disabled' : '',
      cutPaths?.has(entry.path) ? 'is-cut' : '',
      entry.isDir && dropTarget === entry.path ? 'is-drop' : '',
    ].filter(Boolean).join(' '),
    onClick: (event: ReactMouseEvent) => { selectClick(entry, event); if (picking && !entry.isDir && !event.shiftKey && !event.metaKey && !event.ctrlKey) togglePick(entry); },
    onDoubleClick: () => { if (!(picking && !entry.isDir)) open(entry); },
    onContextMenu: (event: ReactMouseEvent) => openMenu(event, entry),
    ...touchMenuProps(entry),
    onDragStart: (event: ReactDragEvent) => onRowDragStart(event, entry),
    ...(entry.isDir ? folderDropProps(entry.path) : {}),
  });

  const sortHeader = (key: 'name' | WebDavColumn, label: string) => (
    <span key={key} role="columnheader" className={`files-col files-col-${key}`} aria-sort={sort.key === key ? (sort.desc ? 'descending' : 'ascending') : 'none'}>
      <button type="button" className="files-sort" onClick={() => changeSort(key)}>
        {label}{sort.key === key && <span aria-hidden="true" className="files-sort-arrow">{sort.desc ? '↓' : '↑'}</span>}
      </button>
      <span className="files-col-resize" role="separator" aria-orientation="vertical" aria-label={t('files.resizeColumn', { column: label })} onMouseDown={event => startResize(event, key)} onDoubleClick={() => updateView({ ...layout, widths: Object.fromEntries(Object.entries(layout.widths).filter(([column]) => column !== key)) })} />
    </span>
  );
  const cell = (entry: WebDavEntry, column: WebDavColumn) => {
    if (column === 'size') return <span key={column} className="files-col files-col-size">{entry.isDir ? '—' : formatSize(entry.size)}</span>;
    if (column === 'modified') return <span key={column} className="files-col files-col-modified">{entry.modified ? dateFormat.format(new Date(entry.modified)) : ''}</span>;
    if (column === 'type') return <span key={column} className="files-col files-col-type">{typeLabel(entry, t('files.folder'))}</span>;
    const parent = parentPath(entry.path);
    return <span key={column} className="files-col files-col-location"><button type="button" className="files-location-link" title={parent} onClick={event => { event.stopPropagation(); navigate(parent); }}>{parent === '/' ? server.displayName : parent.slice(1, -1)}</button></span>;
  };

  return <div className={`files-explorer${picking ? ' files-picking' : ''}`} data-testid="file-explorer">
    <div className="files-toolbar">
      <div className="files-nav">
        <Button variant="ghost" className="files-icon-button files-up" aria-label={t('files.up')} title={t('files.up')} disabled={path === '/'} onClick={() => navigate(parentPath(path))}><UpIcon /></Button>
        <nav className="files-breadcrumbs" aria-label={t('files.breadcrumbs')}>
          <button type="button" className={`files-crumb${path === '/' ? ' is-current' : ''}${dropTarget === '/' && path !== '/' ? ' is-drop' : ''}`} onClick={() => navigate('/')} {...folderDropProps('/')}>
            <span className="files-crumb-icon"><ServerIcon /></span>{server.displayName}
          </button>
          {crumbs(path).map((crumb, index, list) => <span key={crumb.path} className="files-crumb-wrap">
            <span className="files-crumb-sep" aria-hidden="true">›</span>
            <button type="button" aria-current={index === list.length - 1 ? 'page' : undefined} className={`files-crumb${index === list.length - 1 ? ' is-current' : ''}${dropTarget === crumb.path && index !== list.length - 1 ? ' is-drop' : ''}`} onClick={() => navigate(crumb.path)} {...folderDropProps(crumb.path)}>{crumb.name}</button>
          </span>)}
        </nav>
      </div>
      <div className="files-actions">
        <Button variant="ghost" className="files-icon-button" aria-label={t('files.refresh')} title={t('files.refresh')} onClick={() => refresh()} disabled={loading}><RefreshIcon /></Button>
        <div className="files-search">
          <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.75" aria-hidden="true"><circle cx="11" cy="11" r="8" /><path d="m21 21-4.4-4.4" /></svg>
          <input type="search" value={filter} onChange={event => setFilter(event.target.value)} onKeyDown={event => { if (event.key === 'Escape' && filter) { event.preventDefault(); event.stopPropagation(); setFilter(''); } }} placeholder={t('files.search')} aria-label={t('files.search')} data-testid="files-filter" />
          {searching && <span className="files-spinner files-spinner-small files-search-spinner" aria-label={t('files.searching')} />}
          {filter && <button type="button" className="files-search-clear" data-testid="files-search-clear" aria-label={t('files.clearSearch')} title={t('files.clearSearch')} onClick={() => { setFilter(''); listRef.current?.focus(); }}>
            <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" aria-hidden="true"><path d="M18 6 6 18M6 6l12 12" /></svg>
          </button>}
        </div>
        <div role="group" className="files-segments" aria-label={t('files.view')}>
          <button type="button" aria-pressed={view === 'list'} aria-label={t('files.listView')} title={t('files.listView')} onClick={() => changeView('list')}><ListIcon /></button>
          <button type="button" aria-pressed={view === 'grid'} aria-label={t('files.gridView')} title={t('files.gridView')} onClick={() => changeView('grid')}><GridIcon /></button>
          {view === 'list' && <button type="button" className="files-columns-button" aria-label={t('files.columns')} title={t('files.columns')} aria-haspopup="menu" aria-expanded={menu?.kind === 'columns'} data-testid="files-columns" onClick={event => {
          if (menu?.kind === 'columns') { setMenu(null); return; }
          const rect = event.currentTarget.getBoundingClientRect();
          setMenu({ x: rect.left, y: rect.bottom + 4, target: null, kind: 'columns' });
          }}><SlidersIcon /></button>}
        </div>
        {!picking && <>
          <Button onClick={() => openNameDialog({ kind: 'mkdir' })} disabled={busy} data-testid="files-new-folder" aria-label={t('files.newFolder')} title={t('files.newFolder')}><FolderPlusIcon /><span className="files-button-label">{t('files.newFolder')}</span></Button>
          <Button variant="primary" onClick={() => fileInputRef.current?.click()} disabled={busy} data-testid="files-upload" aria-label={t('files.upload')} title={t('files.upload')}><UploadIcon /><span className="files-button-label">{t('files.upload')}</span></Button>
          <input ref={fileInputRef} type="file" multiple hidden onChange={event => { void uploadFiles([...(event.target.files || [])]); event.target.value = ''; }} />
        </>}
      </div>
    </div>

    {error && <div role="alert" className="ui-alert files-alert">{error}<Button variant="ghost" onClick={() => setError(null)} aria-label={t('files.dismiss')}>×</Button></div>}

    <div
      ref={listRef}
      tabIndex={0}
      role="grid"
      aria-label={t('files.contents')}
      aria-busy={loading}
      aria-multiselectable
      className={`files-surface${dropTarget === path ? ' is-drop' : ''}`}
      style={{ '--files-cols': gridTemplate } as CSSProperties}
      onKeyDown={onKeyDown}
      onClick={event => { if (event.target === event.currentTarget || (event.target as Element).classList.contains('files-grid') || (event.target as Element).classList.contains('files-rows')) setSelected(new Set()); }}
      onContextMenu={event => openMenu(event, null)}
      {...surfaceDrop}
    >
      {view === 'list' && visible.length > 0 && <div className="files-header" role="row">
        {picking && <span className="files-check-col" />}
        {sortHeader('name', t('files.name'))}
        {columns.map(column => sortHeader(column, columnLabels[column]))}
      </div>}
      {loading && !entries.length && <div className="files-loading" role="status"><span className="files-spinner" />{t('files.loading')}</div>}
      {!loading && !searching && !error && !visible.length && <EmptyState title={filter ? t('files.noMatches') : t('files.emptyFolder')}>{!filter && !picking ? t('files.emptyFolderHint') : null}</EmptyState>}
      {view === 'list' ? <div className="files-rows">
        {visible.map(entry => <div key={entry.path} {...rowProps(entry)}>
          {picking && <span className="files-check-col">{!entry.isDir && <input type="checkbox" tabIndex={-1} aria-label={t('files.pick', { name: entry.name })} checked={picked.has(entry.path)} disabled={tooLarge(entry)} onChange={() => togglePick(entry)} onClick={event => event.stopPropagation()} />}</span>}
          <span className="files-col files-col-name"><FileIcon kind={fileKind(entry)} /><span className="files-name" title={entry.name}>{entry.name}</span></span>
          {columns.map(column => cell(entry, column))}
        </div>)}
      </div> : <div className="files-grid">
        {visible.map(entry => <div key={entry.path} {...rowProps(entry)} title={entry.name}>
          {picking && !entry.isDir && <input type="checkbox" className="files-tile-check" tabIndex={-1} aria-label={t('files.pick', { name: entry.name })} checked={picked.has(entry.path)} disabled={tooLarge(entry)} onChange={() => togglePick(entry)} onClick={event => event.stopPropagation()} />}
          <span className="files-tile-icon">{fileKind(entry) === 'image' && previewable(entry) ? <img src={api.webdav.fileUrl(server.id, entry.path, true)} alt="" loading="lazy" draggable={false} /> : <FileIcon kind={fileKind(entry)} size={40} />}</span>
          <span className="files-tile-name">{entry.name}</span>
          <span className="files-tile-meta">{entry.isDir ? t('files.folder') : formatSize(entry.size)}</span>
        </div>)}
      </div>}
      {dropTarget === path && <div className="files-drop-hint" aria-hidden="true"><UploadIcon />{t('files.dropToUpload', { folder: path === '/' ? server.displayName : crumbs(path).at(-1)?.name })}</div>}
    </div>

    <div className="files-status" aria-live="polite">
      <span>{searchActive ? t('files.searchResults', { count: visible.length }) : t('files.itemCount', { count: visible.length })}</span>
      {searchActive && searchResult?.truncated && <span>· {t('files.searchTruncated')}</span>}
      {selectedEntries.length > 0 && <span>· {t('files.selectedCount', { count: selectedEntries.length })}{totalSelectedSize > 0 ? ` · ${formatSize(totalSelectedSize)}` : ''}</span>}
      {picking && picked.size > 0 && <span className="files-status-picked">· {t('files.pickedCount', { count: picked.size })}</span>}
      {notice && <span className="files-status-notice">{notice}</span>}
      {busy && <span className="files-spinner files-spinner-small" aria-label={t('files.working')} />}
    </div>

    {uploads.length > 0 && <div className="files-uploads" role="status" aria-label={t('files.uploads')}>
      <div className="files-uploads-head"><strong>{t('files.uploads')}</strong><Button variant="ghost" aria-label={t('files.dismiss')} onClick={() => setUploads(current => current.filter(item => !item.done))}>×</Button></div>
      {uploads.map(item => <div key={item.id} className={`files-upload${item.error ? ' has-error' : ''}`}>
        <span className="files-upload-name" title={item.name}>{item.name}</span>
        {item.error ? <small>{item.error}</small> : <span className="files-progress"><span style={{ width: `${Math.round(item.progress * 100)}%` }} /></span>}
      </div>)}
    </div>}

    {menu && menuItems.length > 0 && <FloatingMenu focusFirst={menu.kind !== 'columns'} x={menu.x} y={menu.y} title={menu.kind === 'columns' ? t('files.columns') : menu.target ? (selectedEntries.length > 1 ? t('files.selectedCount', { count: selectedEntries.length }) : menu.target.name) : (path === '/' ? server.displayName : crumbs(path).at(-1)?.name ?? '')}>
      {menuItems.map(item => 'separator' in item && item.separator
        ? <div key={item.key} className="files-menu-sep" role="separator" />
        : 'checked' in item
          ? <button key={item.key} type="button" role="menuitemcheckbox" aria-checked={item.checked} className="app-menu-item" onClick={() => item.onClick()}>
            <span className="files-menu-check" aria-hidden="true">{item.checked ? <CheckIcon /> : null}</span><span className="files-menu-label">{item.label}</span>
          </button>
          : <button key={item.key} type="button" role="menuitem" className={`app-menu-item${'danger' in item && item.danger ? ' danger' : ''}`} onClick={() => { setMenu(null); void (item as { onClick: () => unknown }).onClick(); }}>
          {(item as { icon?: React.ReactNode }).icon}<span className="files-menu-label">{(item as { label: string }).label}</span>{'hint' in item && typeof item.hint === 'string' && <kbd>{item.hint}</kbd>}
        </button>)}
    </FloatingMenu>}

    {nameDialog && <Dialog testId="files-name-dialog" title={nameDialog.kind === 'mkdir' ? t('files.newFolder') : t('files.renameTitle', { name: nameDialog.entry.name })} closeLabel={t('files.cancel')} onClose={() => setNameDialog(null)} footer={<>
      <Button onClick={() => setNameDialog(null)}>{t('files.cancel')}</Button>
      <Button variant="primary" disabled={invalidName(nameValue)} onClick={() => { void submitName(); }}>{nameDialog.kind === 'mkdir' ? t('files.create') : t('files.rename')}</Button>
    </>}>
      <form className="ui-form" onSubmit={event => { event.preventDefault(); void submitName(); }}>
        <label>{t('files.name')}<NameInput value={nameValue} onChange={setNameValue} selectStem={nameDialog.kind === 'rename' && !nameDialog.entry.isDir} /></label>
        {nameValue && invalidName(nameValue) && <p className="settings-choice-description">{t('files.invalidName')}</p>}
      </form>
    </Dialog>}

    {confirmDelete && <Dialog testId="files-delete-dialog" title={confirmDelete.length > 1 ? t('files.deleteCount', { count: confirmDelete.length }) : t('files.deleteTitle', { name: confirmDelete[0].name })} closeLabel={t('files.cancel')} onClose={() => setConfirmDelete(null)} footer={<>
      <Button onClick={() => setConfirmDelete(null)}>{t('files.cancel')}</Button>
      <Button variant="danger" autoFocus onClick={() => { void remove(confirmDelete); }}>{t('files.delete')}</Button>
    </>}>
      <p className="settings-choice-description">{confirmDelete.some(entry => entry.isDir) ? t('files.deleteFolderWarning') : t('files.deleteWarning')}</p>
      <ul className="files-delete-list">{confirmDelete.slice(0, 8).map(entry => <li key={entry.path}><FileIcon kind={fileKind(entry)} size={14} />{entry.name}</li>)}{confirmDelete.length > 8 && <li>… {t('files.andMore', { count: confirmDelete.length - 8 })}</li>}</ul>
    </Dialog>}

    {preview && <PreviewDialog server={server} entry={preview} siblings={visible.filter(item => !item.isDir && previewable(item))} onNavigate={setPreview} onDownload={() => download([preview])} onClose={() => setPreview(null)} />}
  </div>;
}

function NameInput({ value, onChange, selectStem }: { value: string; onChange: (value: string) => void; selectStem: boolean }) {
  const ref = useRef<HTMLInputElement | null>(null);
  useLayoutEffect(() => {
    const input = ref.current;
    if (!input) return;
    input.focus();
    const dot = input.value.lastIndexOf('.');
    input.setSelectionRange(0, selectStem && dot > 0 ? dot : input.value.length);
  }, [selectStem]);
  return <input ref={ref} autoFocus value={value} maxLength={255} onChange={event => onChange(event.target.value)} style={inputStyle} data-testid="files-name-input" />;
}

/** A context menu at the pointer, kept inside the viewport. */
function FloatingMenu({ x, y, title, children, focusFirst = true }: { x: number; y: number; title: string; children: React.ReactNode; focusFirst?: boolean }) {
  const ref = useRef<HTMLDivElement | null>(null);
  // Pointer coordinates are viewport pixels; the menu sits inside the UI-scaled app, so its
  // CSS offsets are divided by the scale (like the mail context menu).
  const scale = useUiScale();
  const [position, setPosition] = useState<CSSProperties>({ left: x / scale, top: y / scale, visibility: 'hidden' });
  useLayoutEffect(() => {
    const menu = ref.current;
    if (!menu) return;
    const { width, height } = menu.getBoundingClientRect();
    const left = Math.max(8, Math.min(x, window.innerWidth - width - 8));
    const top = Math.max(8, Math.min(y, window.innerHeight - height - 8));
    setPosition({ left: left / scale, top: top / scale });
    if (focusFirst) menu.querySelector<HTMLElement>('[role=menuitem]')?.focus();
  }, [x, y, scale, focusFirst]);
  const onKeyDown = (event: ReactKeyboardEvent) => {
    if (event.key !== 'ArrowDown' && event.key !== 'ArrowUp') return;
    event.preventDefault();
    const items = [...(ref.current?.querySelectorAll<HTMLElement>('[role=menuitem], [role=menuitemcheckbox]') ?? [])];
    const index = items.indexOf(document.activeElement as HTMLElement);
    items[(index + (event.key === 'ArrowDown' ? 1 : -1) + items.length) % items.length]?.focus();
  };
  return <div ref={ref} role="menu" aria-label={title} className="app-menu files-menu" style={{ position: 'fixed', zIndex: 4600, minWidth: 210, ...position }} onKeyDown={onKeyDown} onContextMenu={event => event.preventDefault()}>
    <div className="app-menu-title">{title}</div>
    {children}
  </div>;
}

function PreviewDialog({ server, entry, siblings, onNavigate, onDownload, onClose }: { server: WebDavServer; entry: WebDavEntry; siblings: WebDavEntry[]; onNavigate: (entry: WebDavEntry) => void; onDownload: () => void; onClose: () => void }) {
  const { t } = useTranslation();
  const kind = fileKind(entry);
  const src = api.webdav.fileUrl(server.id, entry.path, true);
  const index = siblings.findIndex(item => item.path === entry.path);
  const [text, setText] = useState<string | null>(null);
  useEffect(() => {
    if (kind !== 'text') return;
    const controller = new AbortController();
    setText(null);
    fetch(src, { credentials: 'include', signal: controller.signal })
      .then(response => response.ok ? response.text() : Promise.reject(new Error(String(response.status))))
      .then(body => setText(body.length > 400_000 ? `${body.slice(0, 400_000)}\n…` : body))
      .catch(() => { if (!controller.signal.aborted) setText(t('files.previewFailed')); });
    return () => controller.abort();
  }, [src, kind, t]);
  useEffect(() => {
    const key = (event: KeyboardEvent) => {
      if (event.target instanceof HTMLInputElement) return;
      if (event.key === 'ArrowRight' && index >= 0 && index < siblings.length - 1) onNavigate(siblings[index + 1]);
      if (event.key === 'ArrowLeft' && index > 0) onNavigate(siblings[index - 1]);
    };
    document.addEventListener('keydown', key);
    return () => document.removeEventListener('keydown', key);
  }, [index, siblings, onNavigate]);
  return <Dialog testId="files-preview" className="files-preview-dialog" title={entry.name} closeLabel={t('files.close')} onClose={onClose} footer={<>
    <span className="files-preview-meta">{formatSize(entry.size)}{siblings.length > 1 && index >= 0 ? ` · ${index + 1} / ${siblings.length}` : ''}</span>
    <Button variant="ghost" aria-label={t('files.previous')} disabled={index <= 0} onClick={() => onNavigate(siblings[index - 1])}>‹</Button>
    <Button variant="ghost" aria-label={t('files.next')} disabled={index < 0 || index >= siblings.length - 1} onClick={() => onNavigate(siblings[index + 1])}>›</Button>
    <Button variant="primary" onClick={onDownload}><DownloadIcon />{t('files.download')}</Button>
  </>}>
    <div className="files-preview-body">
      {kind === 'image' && <img src={src} alt={entry.name} />}
      {kind === 'pdf' && <iframe src={src} title={entry.name} />}
      {kind === 'video' && <video src={src} controls autoPlay />}
      {kind === 'audio' && <audio src={src} controls autoPlay />}
      {kind === 'text' && (text === null ? <div className="files-loading"><span className="files-spinner" /></div> : <pre>{text}</pre>)}
    </div>
  </Dialog>;
}

// 14px stroke icons in the menu-icon style.
const icon = { width: 14, height: 14, viewBox: '0 0 24 24', fill: 'none', stroke: 'currentColor', strokeWidth: 1.75, strokeLinecap: 'round' as const, strokeLinejoin: 'round' as const, 'aria-hidden': true };
const OpenIcon = () => <svg {...icon}><path d="M15 3h6v6" /><path d="M10 14 21 3" /><path d="M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6" /></svg>;
const DownloadIcon = () => <svg {...icon}><path d="M12 3v12" /><path d="m7 10 5 5 5-5" /><path d="M5 21h14" /></svg>;
const UploadIcon = () => <svg {...icon}><path d="M12 21V9" /><path d="m7 14 5-5 5 5" /><path d="M5 3h14" /></svg>;
const CopyIcon = () => <svg {...icon}><rect x="9" y="9" width="12" height="12" rx="2" /><path d="M5 15H4a1 1 0 0 1-1-1V4a1 1 0 0 1 1-1h10a1 1 0 0 1 1 1v1" /></svg>;
const CutIcon = () => <svg {...icon}><circle cx="6" cy="6" r="3" /><circle cx="6" cy="18" r="3" /><path d="M20 4 8.12 15.88M14.47 14.48 20 20M8.12 8.12 12 12" /></svg>;
const PasteIcon = () => <svg {...icon}><rect x="8" y="2" width="8" height="4" rx="1" /><path d="M16 4h2a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2h2" /></svg>;
const LinkIcon = () => <svg {...icon}><path d="M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71" /><path d="M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71" /></svg>;
const FolderPlusIcon = () => <svg {...icon}><path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2Z" /><path d="M12 11v5M9.5 13.5h5" /></svg>;
const SelectIcon = () => <svg {...icon}><rect x="3" y="3" width="18" height="18" rx="2" /><path d="m8 12 3 3 5-6" /></svg>;
const RefreshIcon = () => <svg {...icon}><path d="M20 7v5h-5M4 17v-5h5" /><path d="M5.6 7a8 8 0 0 1 13.2-1L20 8M4 16l1.2 2A8 8 0 0 0 18.4 17" /></svg>;
const UpIcon = () => <svg {...icon}><path d="M12 19V5" /><path d="m5 12 7-7 7 7" /></svg>;
const ListIcon = () => <svg {...icon}><path d="M8 6h13M8 12h13M8 18h13M3 6h.01M3 12h.01M3 18h.01" /></svg>;
const GridIcon = () => <svg {...icon}><rect x="3" y="3" width="7" height="7" rx="1" /><rect x="14" y="3" width="7" height="7" rx="1" /><rect x="3" y="14" width="7" height="7" rx="1" /><rect x="14" y="14" width="7" height="7" rx="1" /></svg>;
const SlidersIcon = () => <svg {...icon}><path d="M4 6h10M18 6h2M4 12h4M12 12h8M4 18h12M20 18h0" /><circle cx="16" cy="6" r="2" /><circle cx="10" cy="12" r="2" /><circle cx="18" cy="18" r="2" /></svg>;
const CheckIcon = () => <svg {...icon}><path d="m5 12 5 5 9-10" /></svg>;
const FolderIcon = () => <svg {...icon}><path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2Z" /></svg>;
export const ServerIcon = () => <svg {...icon}><rect x="3" y="4" width="18" height="7" rx="2" /><rect x="3" y="13" width="18" height="7" rx="2" /><path d="M7 7.5h.01M7 16.5h.01" /></svg>;
