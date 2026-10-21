import { useBackLayer } from '../hooks/useBackNavigation.ts';
import type { CalendarViewEvent } from './calendarView';
import { isWritableCalendarSource } from './calendarView';
import type { TFunction } from 'i18next';
import { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react';
import type { CSSProperties } from 'react';
import { EditIcon, TrashIcon } from './menuIcons.tsx';

export default function CalendarContextMenu({ x, y, event, isMobile = false, onEdit, onDelete, onClose, triggerRef, t }: {
  x: number;
  y: number;
  event: CalendarViewEvent;
  isMobile?: boolean;
  onEdit: () => void;
  onDelete: () => void;
  onClose: () => void;
  triggerRef?: { current: unknown };
  t: TFunction;
}) {
  const menuRef = useRef<HTMLDivElement>(null);
  useBackLayer(true, onClose, 4000);
  const onCloseRef = useRef(onClose);
  const [position, setPosition] = useState({ x, y });
  const writable = isWritableCalendarSource(event.source) && !event.read_only;
  const restoreFocus = useCallback(() => requestAnimationFrame(() => {
    if (triggerRef) {
      const trigger = triggerRef.current;
      if (trigger instanceof HTMLElement) trigger.focus();
    }
  }), [triggerRef]);
  onCloseRef.current = onClose;

  useLayoutEffect(() => {
    if (isMobile || !menuRef.current) return;
    const rect = menuRef.current.getBoundingClientRect();
    setPosition({
      x: Math.max(8, Math.min(x, window.innerWidth - rect.width - 8)),
      y: Math.max(8, Math.min(y, window.innerHeight - rect.height - 8)),
    });
  }, [x, y, isMobile]);

  useEffect(() => {
    const handleKeyDown = (keyboardEvent: KeyboardEvent) => {
      if (keyboardEvent.key === 'Escape') {
        keyboardEvent.preventDefault();
        onCloseRef.current();
        restoreFocus();
        return;
      }
      if (!writable || !menuRef.current) return;
      const items = [...menuRef.current.querySelectorAll('[role="menuitem"]')];
      const activeElement = document.activeElement;
      let currentIndex = -1;
      if (activeElement) currentIndex = items.indexOf(activeElement);
      let nextIndex;
      if (keyboardEvent.key === 'ArrowDown') nextIndex = (currentIndex + 1 + items.length) % items.length;
      if (keyboardEvent.key === 'ArrowUp') nextIndex = (currentIndex - 1 + items.length) % items.length;
      if (keyboardEvent.key === 'Home') nextIndex = 0;
      if (keyboardEvent.key === 'End') nextIndex = items.length - 1;
      if (nextIndex === undefined) return;
      keyboardEvent.preventDefault();
      const nextItem = items[nextIndex];
      if (nextItem instanceof HTMLElement) nextItem.focus();
    };
    document.addEventListener('keydown', handleKeyDown);
    const menuElement = menuRef.current;
    if (menuElement) {
      const firstButton = menuElement.querySelector('button');
      if (firstButton instanceof HTMLButtonElement) firstButton.focus();
    }
    return () => document.removeEventListener('keydown', handleKeyDown);
  }, [triggerRef, writable, restoreFocus]);

  const run = (action: () => void, restore = true) => {
    action();
    onCloseRef.current();
    if (restore) restoreFocus();
  };
  const closeFromOutside = () => {
    onCloseRef.current();
    restoreFocus();
  };
  return <>
    <button type="button" aria-label={t('calendar.closeMenu', 'Close calendar menu')} onClick={closeFromOutside} style={scrim} />
    <div ref={menuRef} role="menu" aria-label={t('calendar.eventActions', 'Event actions')} data-testid="calendar-context-menu" className="app-menu" style={{ ...menu, ...(isMobile ? mobileMenu : { left: position.x, top: position.y }) }}>
      <div className="app-menu-title">{event.summary || t('calendar.untitled')}</div>
      {writable && <>
        <button type="button" role="menuitem" className="app-menu-item" onClick={() => run(onEdit, false)}><EditIcon />{t('calendar.edit')}</button>
        <button type="button" role="menuitem" className="app-menu-item danger" data-testid="calendar-context-delete" onClick={() => run(onDelete)}><TrashIcon />{t('calendar.delete')}</button>
      </>}
      {!writable && <span role="status" data-testid="calendar-context-read-only" className="app-menu-note">{t('calendar.readOnly')}</span>}
    </div>
  </>;
}

const scrim: CSSProperties = { position: 'fixed', inset: 0, zIndex: 3999, border: 0, background: 'transparent', padding: 0, cursor: 'default' };
// Look comes from the shared .app-menu classes (ui.css); only placement lives here.
const menu: CSSProperties = { position: 'fixed', zIndex: 4000, width: 220, maxWidth: 'calc(100vw - 16px)', boxSizing: 'border-box' };
const mobileMenu: CSSProperties = { position: 'fixed', left: 12, right: 12, bottom: 'calc(var(--mobile-nav-height) + var(--sab) + 12px)', width: 'auto' };
