// The 14px stroke icons calendar menus share with the mail context menu's item style.
const iconProps = { width: 14, height: 14, viewBox: '0 0 24 24', fill: 'none', stroke: 'currentColor', strokeWidth: 1.75, strokeLinecap: 'round' as const, strokeLinejoin: 'round' as const, 'aria-hidden': true };

export const EditIcon = () => <svg {...iconProps}><path d="M12 20h9"/><path d="M16.5 3.5a2.12 2.12 0 0 1 3 3L7 19l-4 1 1-4Z"/></svg>;
export const TrashIcon = () => <svg {...iconProps}><path d="M3 6h18"/><path d="M8 6V4h8v2"/><path d="M19 6l-1 14H6L5 6"/><path d="M10 11v6M14 11v6"/></svg>;
export const PaletteIcon = () => <svg {...iconProps}><circle cx="13.5" cy="6.5" r="1"/><circle cx="17.5" cy="10.5" r="1"/><circle cx="8.5" cy="7.5" r="1"/><circle cx="6.5" cy="12.5" r="1"/><path d="M12 2a10 10 0 1 0 0 20c1 0 1.5-.8 1.5-1.6 0-.4-.2-.8-.4-1.1a1.6 1.6 0 0 1 1.2-2.7H16a6 6 0 0 0 6-6C22 6 17.5 2 12 2Z"/></svg>;
