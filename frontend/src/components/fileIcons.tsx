// File-type icons for the WebDAV explorer: one stroke glyph per family, tinted by type.
import type { WebDavEntry } from '../utils/api.ts';

export type FileKind = 'folder' | 'image' | 'video' | 'audio' | 'pdf' | 'archive' | 'code' | 'sheet' | 'slides' | 'doc' | 'text' | 'file';

const EXTENSIONS: Record<Exclude<FileKind, 'folder' | 'file'>, string[]> = {
  image: ['png', 'jpg', 'jpeg', 'gif', 'webp', 'bmp', 'svg', 'heic', 'heif', 'tif', 'tiff', 'avif', 'ico'],
  video: ['mp4', 'mov', 'mkv', 'webm', 'avi', 'm4v', 'wmv'],
  audio: ['mp3', 'wav', 'flac', 'ogg', 'm4a', 'aac', 'opus'],
  pdf: ['pdf'],
  archive: ['zip', 'tar', 'gz', 'tgz', 'bz2', 'xz', '7z', 'rar', 'zst'],
  code: ['js', 'ts', 'tsx', 'jsx', 'json', 'html', 'css', 'scss', 'py', 'rs', 'go', 'java', 'c', 'cpp', 'h', 'sh', 'yml', 'yaml', 'toml', 'xml', 'sql', 'php', 'rb'],
  sheet: ['xls', 'xlsx', 'ods', 'csv', 'numbers'],
  slides: ['ppt', 'pptx', 'odp', 'key'],
  doc: ['doc', 'docx', 'odt', 'rtf', 'pages'],
  text: ['txt', 'md', 'log', 'ini', 'conf', 'cfg'],
};

export function extensionOf(name: string): string {
  const dot = name.lastIndexOf('.');
  return dot > 0 ? name.slice(dot + 1).toLowerCase() : '';
}

export function fileKind(entry: Pick<WebDavEntry, 'name' | 'isDir' | 'contentType'>): FileKind {
  if (entry.isDir) return 'folder';
  const ext = extensionOf(entry.name);
  for (const [kind, list] of Object.entries(EXTENSIONS)) if (list.includes(ext)) return kind as FileKind;
  const type = entry.contentType || '';
  if (type.startsWith('image/')) return 'image';
  if (type.startsWith('video/')) return 'video';
  if (type.startsWith('audio/')) return 'audio';
  if (type.startsWith('text/')) return 'text';
  return 'file';
}

/** Kinds the preview dialog can show inline. */
export function previewable(entry: Pick<WebDavEntry, 'name' | 'isDir' | 'contentType'>): boolean {
  const kind = fileKind(entry);
  if (kind === 'image') return extensionOf(entry.name) !== 'svg' && !['heic', 'heif', 'tif', 'tiff'].includes(extensionOf(entry.name));
  return kind === 'pdf' || kind === 'video' || kind === 'audio' || kind === 'text';
}

const COLORS: Record<FileKind, string> = {
  folder: 'var(--accent)',
  image: '#c084fc',
  video: '#f472b6',
  audio: '#fb923c',
  pdf: '#f87171',
  archive: '#facc15',
  code: '#34d399',
  sheet: '#4ade80',
  slides: '#fb923c',
  doc: '#60a5fa',
  text: 'var(--text-secondary)',
  file: 'var(--text-tertiary)',
};

export function FileIcon({ kind, size = 18 }: { kind: FileKind; size?: number }) {
  const common = { width: size, height: size, viewBox: '0 0 24 24', fill: 'none', stroke: COLORS[kind], strokeWidth: 1.75, strokeLinecap: 'round' as const, strokeLinejoin: 'round' as const, 'aria-hidden': true, style: { flexShrink: 0 } };
  if (kind === 'folder') {
    return <svg {...common}><path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2Z" fill="color-mix(in srgb, var(--accent) 22%, transparent)" /></svg>;
  }
  const sheet = <><path d="M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8Z" /><path d="M14 3v5h5" /></>;
  const marks: Partial<Record<FileKind, React.ReactNode>> = {
    image: <><circle cx="10" cy="12" r="1.5" /><path d="m19 17-4-4-7 7" /></>,
    video: <path d="m10 11 4 2.5-4 2.5Z" />,
    audio: <><path d="M10 17v-5l4-1v5" /><circle cx="9" cy="17" r="1" /><circle cx="13" cy="16" r="1" /></>,
    pdf: <path d="M9 13h6M9 16h4" />,
    archive: <path d="M11 9h1M12 11h1M11 13h1M12 15h1" />,
    code: <path d="m10 12-2 2 2 2M14 12l2 2-2 2" />,
    sheet: <path d="M8 12h8M8 15h8M12 12v6" />,
    slides: <path d="M8 12h8v5H8Z" />,
    doc: <path d="M8 12h8M8 15h8M8 18h5" />,
    text: <path d="M8 12h8M8 15h6" />,
  };
  return <svg {...common}>{sheet}{marks[kind]}</svg>;
}
