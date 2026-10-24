import { useEffect, useRef, useState } from 'react';
import type { CSSProperties, ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { useBackLayer } from '../hooks/useBackNavigation.ts';
import { useUiScale, descale } from '../hooks/useUiScale.ts';

// Address chips with contact suggestions — the recipient fields of the composer and the
// attendee field of calendar events. Suggestions come from the caller (contacts, people
// mailed with); a chip is "Name <address>" or a bare address.
export type ContactSuggestion = { email: string; name?: string | null };

/** The bare address of a chip ("Name <a@b.c>" → "a@b.c"). */
export function chipAddress(chip: string): string {
  const match = (chip || '').match(/<([^>]+)>/);
  return (match ? match[1] : chip || '').trim();
}

export default function ChipInput({ chips, onChipsChange, value, onChange, placeholder, autoFocus = false, inputStyle, getSuggestions, containerStyle = undefined, chipAdornment, disabled = false, testId, lockedChip, onChipClick }: { chips: string[]; onChipsChange: (chips: string[]) => void; value: string; onChange: (value: string) => void; placeholder?: string; autoFocus?: boolean; inputStyle?: CSSProperties; getSuggestions?: (query: string) => Promise<ContactSuggestion[]> | ContactSuggestion[]; containerStyle?: CSSProperties; /** Extra content after a chip's label, e.g. an attendee's answer. */ chipAdornment?: (chip: string) => ReactNode; disabled?: boolean; testId?: string; /** A chip that can't be edited or removed (e.g. the user's own attendee entry). */ lockedChip?: (chip: string) => boolean; /** Click on a chip, at the pointer position (e.g. to open a menu for it). */ onChipClick?: (chip: string, x: number, y: number) => void }) {
  const { t } = useTranslation();
  const uiScale = useUiScale();
  const [suggestions, setSuggestions] = useState<ContactSuggestion[]>([]);
  const [suggIdx, setSuggIdx] = useState(-1);
  const debounceRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const wrapperRef = useRef<HTMLDivElement | null>(null);
  const inputRef = useRef<HTMLInputElement | null>(null);
  const [dropStyle, setDropStyle] = useState<Record<string, unknown> | null>(null);
  const [menu, setMenu] = useState<{ x: number; y: number; index: number } | null>(null); // { x, y, index } | null — recipient chip context menu
  useBackLayer(menu, () => setMenu(null), 2200);
  const longPressRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  // Debounce contact suggestions — only when getSuggestions is wired up
  useEffect(() => {
    if (!getSuggestions) return;
    if (debounceRef.current !== null) clearTimeout(debounceRef.current);
    const q = value.trim();
    if (q.length < 2) { setSuggestions([]); setSuggIdx(-1); setDropStyle(null); return; }
    debounceRef.current = setTimeout(async () => {
      try {
        const results = await getSuggestions(q);
        if (!results.length) { setSuggestions([]); setDropStyle(null); return; }
        // Measure wrapper position for the fixed dropdown — escapes overflow:auto containers
        if (wrapperRef.current) {
          const rect = wrapperRef.current.getBoundingClientRect();
          // Cap the height so a long list overflows and scrolls inside its own box
          // rather than extending behind the mobile keyboard (which lets the drag
          // fall through and scroll the page underneath).
          const avail = (window.visualViewport?.height || window.innerHeight) - rect.bottom - 16;
          setDropStyle({ top: rect.bottom + 2, left: rect.left, width: Math.max(rect.width, 220), maxHeight: Math.max(120, Math.min(240, avail)) });
        }
        setSuggestions(results);
        setSuggIdx(-1);
      } catch { /* intentional */ }
    }, 200);
    return () => { if (debounceRef.current !== null) clearTimeout(debounceRef.current); };
  }, [value, getSuggestions]);

  const clearSuggestions = () => { setSuggestions([]); setSuggIdx(-1); setDropStyle(null); };

  const commitInput = () => {
    const trimmed = value.trim();
    if (trimmed) { onChipsChange([...chips, trimmed]); onChange(''); }
    clearSuggestions();
  };

  const commitSuggestion = (contact: ContactSuggestion) => {
    const formatted = contact.name ? `${contact.name} <${contact.email}>` : contact.email;
    onChipsChange([...chips, formatted]);
    onChange('');
    clearSuggestions();
  };

  const handleKeyDown = (e: React.KeyboardEvent) => {
    if (suggestions.length > 0) {
      if (e.key === 'ArrowDown') { e.preventDefault(); setSuggIdx(i => Math.min(i + 1, suggestions.length - 1)); return; }
      if (e.key === 'ArrowUp') { e.preventDefault(); setSuggIdx(i => Math.max(i - 1, -1)); return; }
      if (e.key === 'Escape') { clearSuggestions(); return; }
      // Enter/Tab with the dropdown open: use the highlighted suggestion; if none is
      // highlighted, commit a fully-typed email literally, otherwise take the top
      // suggestion. Previously an un-highlighted Enter fell through and committed the
      // raw typed text (e.g. a name like "tommy"), which then failed recipient
      // validation at send time.
      if (e.key === 'Enter' || e.key === 'Tab') {
        e.preventDefault();
        if (suggIdx >= 0) commitSuggestion(suggestions[suggIdx]);
        else if (/^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(value.trim())) commitInput();
        else commitSuggestion(suggestions[0]);
        return;
      }
    }
    if (e.key === ',' || e.key === 'Enter' || e.key === 'Tab') {
      if (value.trim()) { e.preventDefault(); commitInput(); }
    } else if (e.key === 'Backspace' && !value && chips.length && !lockedChip?.(chips[chips.length - 1])) {
      onChipsChange(chips.slice(0, -1));
    }
  };

  // Pull the bare email out of a chip string ("Jane <jane@x.com>" -> "jane@x.com").
  const chipEmail = (chip: string) => {
    const m = (chip || '').match(/<([^>]+)>/);
    return (m ? m[1] : chip || '').trim();
  };
  const copyText = (text: string) => { navigator.clipboard?.writeText(text).catch(() => {}); };

  // Load a chip back into the input for editing, preserving any half-typed text.
  const startEdit = (i: number) => {
    const chipText = chips[i];
    const rest = chips.filter((_, j) => j !== i);
    const pending = value.trim();
    onChipsChange(pending ? [...rest, pending] : rest);
    onChange(chipText);
    clearSuggestions();
    setMenu(null);
    setTimeout(() => inputRef.current?.focus(), 0);
  };

  // Open the chip menu, clamped so it stays within the viewport.
  const openMenu = (clientX: number, clientY: number, index: number) => {
    setMenu({ x: Math.min(clientX, window.innerWidth - 176), y: Math.min(clientY, window.innerHeight - 168), index });
  };
  const onChipTouchStart = (e: React.TouchEvent, i: number) => {
    const touch = e.touches[0];
    if (longPressRef.current !== null) clearTimeout(longPressRef.current);
    longPressRef.current = setTimeout(() => openMenu(touch.clientX, touch.clientY, i), 500);
  };
  const cancelLongPress = () => { if (longPressRef.current !== null) clearTimeout(longPressRef.current); };

  useEffect(() => {
    if (!menu) return;
    const close = () => setMenu(null);
    const onKey = (e: KeyboardEvent) => { if (e.key === 'Escape') setMenu(null); };
    document.addEventListener('pointerdown', close);
    document.addEventListener('keydown', onKey);
    return () => { document.removeEventListener('pointerdown', close); document.removeEventListener('keydown', onKey); };
  }, [menu]);

  const menuLocked = menu ? Boolean(lockedChip?.(chips[menu.index])) : false;
  const menuItems = menu ? [
    ...(menuLocked ? [] : [{ label: t('common.edit'), fn: () => startEdit(menu.index) }]),
    { label: t('compose.recipientMenu.copy'),    fn: () => { copyText(chipEmail(chips[menu.index])); setMenu(null); } },
    { label: t('compose.recipientMenu.copyAll'), fn: () => { copyText(chips.map(chipEmail).join(', ')); setMenu(null); } },
    ...(menuLocked ? [] : [{ label: t('common.remove'), fn: () => { onChipsChange(chips.filter((_, j) => j !== menu.index)); setMenu(null); }, danger: true }]),
  ] : [];

  return (
    <div ref={wrapperRef} data-testid={testId} style={{ display: 'flex', flexWrap: 'wrap', gap: 4, flex: 1, alignItems: 'center', padding: '5px 0', minWidth: 0, ...containerStyle }}>
      {chips.map((chip, i) => { const locked = Boolean(lockedChip?.(chip)); return (
        <span key={i}
          title={chip}
          data-locked={locked || undefined}
          onClick={onChipClick ? (e: React.MouseEvent<HTMLElement>) => onChipClick(chip, e.clientX, e.clientY) : undefined}
          onDoubleClick={() => { if (!locked) startEdit(i); }}
          onContextMenu={ (e: React.MouseEvent<HTMLElement>) => { e.preventDefault(); openMenu(e.clientX, e.clientY, i); }}
          onTouchStart={ (e: React.TouchEvent) => onChipTouchStart(e, i)}
          onTouchEnd={cancelLongPress}
          onTouchMove={cancelLongPress}
          style={{
            display: 'inline-flex', alignItems: 'center', gap: 3,
            background: 'var(--bg-tertiary)', color: 'var(--text-secondary)',
            border: '1px solid var(--border-subtle)',
            borderRadius: 999, padding: '1px 4px 1px 9px', fontSize: 12,
            maxWidth: 220, cursor: onChipClick ? 'pointer' : 'default', userSelect: 'none',
            ...(locked && !disabled ? { paddingRight: 9 } : {}),
          }}>
          <span style={{ overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', minWidth: 0 }}>{chip}</span>
          {chipAdornment?.(chip)}
          {!disabled && !locked && <button
            type="button"
            onClick={() => onChipsChange(chips.filter((_, j) => j !== i))}
            style={{ background: 'none', border: 'none', padding: '1px', cursor: 'pointer', color: 'var(--text-tertiary)', display: 'flex', lineHeight: 1, flexShrink: 0, borderRadius: '50%' }}
          >
            <svg width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5">
              <line x1="18" y1="6" x2="6" y2="18"/><line x1="6" y1="6" x2="18" y2="18"/>
            </svg>
          </button>}
        </span>
      ); })}
      <input
        ref={inputRef}
        type="text"
        value={value}
        onChange={ (e: React.ChangeEvent<HTMLInputElement | HTMLTextAreaElement | HTMLSelectElement>) => onChange(e.target.value)}
        onKeyDown={handleKeyDown}
        onBlur={commitInput}
        placeholder={chips.length ? '' : placeholder}
        autoFocus={autoFocus}
        disabled={disabled}
        style={{ ...inputStyle, flex: '1 1 80px', minWidth: 80 }}
      />
      {suggestions.length > 0 && dropStyle && (
        <div
          onMouseDown={ (e: React.MouseEvent<HTMLElement>) => e.preventDefault()} /* keep the input focused; don't clear on interaction */
          style={{
            position: 'fixed',
            top: descale(dropStyle.top, uiScale), left: descale(dropStyle.left, uiScale), width: descale(dropStyle.width, uiScale),
            maxHeight: descale(dropStyle.maxHeight, uiScale),
            zIndex: 9800,
            background: 'var(--bg-elevated)', border: '1px solid var(--border)',
            borderRadius: 8, boxShadow: 'var(--shadow-popover)',
            overflowY: 'auto', overflowX: 'hidden', touchAction: 'pan-y',
            WebkitOverflowScrolling: 'touch', overscrollBehavior: 'contain',
          }}>
          {suggestions.map((contact, i) => (
            <div
              key={contact.email}
              onClick={() => commitSuggestion(contact)}
              onMouseEnter={() => setSuggIdx(i)}
              style={{
                padding: '8px 12px', cursor: 'pointer',
                background: i === suggIdx ? 'var(--bg-hover)' : 'transparent',
                borderBottom: i < suggestions.length - 1 ? '1px solid var(--border-subtle)' : 'none',
                transition: 'background 0.08s',
              }}
            >
              {contact.name && (
                <div style={{ fontSize: 13, color: 'var(--text-primary)', fontWeight: 500, lineHeight: 1.3 }}>
                  {contact.name}
                </div>
              )}
              <div style={{ fontSize: 11, color: 'var(--text-tertiary)', lineHeight: 1.3 }}>
                {contact.email}
              </div>
            </div>
          ))}
        </div>
      )}
      {menu && (
        <div
          onPointerDown={e => e.stopPropagation()}
          style={{
            position: 'fixed', top: descale(menu.y, uiScale), left: descale(menu.x, uiScale), zIndex: 9900, minWidth: 160,
            background: 'var(--bg-elevated)', border: '1px solid var(--border)',
            borderRadius: 8, boxShadow: 'var(--shadow-popover)', overflow: 'hidden', padding: 4,
          }}
        >
          {menuItems.map(item => (
            <div
              key={item.label}
              onClick={item.fn}
              style={{
                padding: '8px 12px', fontSize: 13, cursor: 'pointer', borderRadius: 6,
                color: item.danger ? 'var(--red)' : 'var(--text-primary)', whiteSpace: 'nowrap',
              }}
              onMouseEnter={ (e: React.MouseEvent<HTMLElement>) => e.currentTarget.style.background = 'var(--bg-hover)'}
              onMouseLeave={ (e: React.MouseEvent<HTMLElement>) => e.currentTarget.style.background = 'transparent'}
            >
              {item.label}
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
