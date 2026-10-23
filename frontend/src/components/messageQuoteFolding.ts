const FORWARD_MARKER_RE = /^(?:[-–—]{2,}\s*)?(?:forwarded message|forwarded mail|begin forwarded message|weitergeleitete nachricht|anfang der weitergeleiteten nachricht|message transféré|début du message réexpédié|mensaje reenviado|messaggio inoltrato|doorgestuurd bericht|vidarebefordrat meddelande|videresendt meddelelse|mensagem encaminhada|przekazana wiadomość|wiadomość przekazana|пересылаемое сообщение)(?:\s*[-–—]{2,})?:?/i;
// Attribution and separator lines that open quoted history, as written by Gmail, Apple Mail,
// Thunderbird, Outlook, Android and webmailers in the common languages (patterns after
// mailgun/talon and unquotemail).
const REPLY_MARKER_RE = new RegExp([
  String.raw`^on\s.+\s(?:wrote|sent)\s*:`,
  String.raw`^dnia\s+.+\s+napisa(?:ł|ła|li|ły)?\(?(?:a)?\)?:`,
  String.raw`^w dniu\s.+\s(?:napisał|napisała|napisał\(a\)|użytkownik.+napisał)`,
  String.raw`^am\s+.+\s+schrieb(?:\s+.+)?:`,
  String.raw`^le\s+.+\s+a écrit\s*:`,
  String.raw`^el\s+.+\s+escribió\s*:`,
  String.raw`^em\s+.+\s+escreveu\s*:`,
  String.raw`^il\s+.+\s+ha scritto\s*:`,
  String.raw`^op\s+.+\s+(?:schreef|verzond|geschreven)(?:\s+.+)?:`,
  String.raw`^(?:den|på)\s+.+\s+skrev(?:\s+.+)?:`,
  String.raw`^vào\s+.+\s+đã viết\s*:`,
  String.raw`^[-–—]{2,}\s*(?:original message|reply message|ursprüngliche nachricht|originalnachricht|antwort nachricht|wiadomość oryginalna|message d'origine|message original|mensaje original|mensagem original|messaggio originale|oorspronkelijk bericht|ursprungligt meddelande|oprindelig meddelelse|opprinnelig melding|alkuperäinen viesti|původní zpráva|исходное сообщение|оригинальное сообщение)\s*[-–—]{2,}`,
  // Android / Samsung: "---- Name wrote ----", "-------- Original message --------".
  String.raw`^[-–—]{2,}.*\b(?:wrote|original message)\b.*[-–—]{2,}$`,
].join('|'), 'i');
// A short attribution line ending in "wrote:" (or a translation) that names a date or address:
// "Max <max@example.test> schrieb:", "2026年10月16日 19:57 Maria <m@x> 写道：".
const WROTE_SUFFIX_RE = /(?:wrote|schrieb|a écrit|escribió|escreveu|ha scritto|schreef|skrev|kirjoitti|napisał\(a\)|napisał|napisała|napsal\(a\)|napsal|napsala|написал\(а\)|написал|написала|пише|yazdı|írta|έγραψε|đã viết|写道|寫道|님이 작성|のメッセージ|が書きました)\s*[:：]$/i;
// "16.10.2026 19:57:53 Maria Tagsold <maria@example.test>:" — a date and time, the sender
// and their address, then a colon (no "wrote"), as some clients and webmailers emit.
const DATED_SENDER_MARKER_RE = /^(?:\S{2,10},?\s+)?\d{1,4}(?:[./-]|年)\s?\d{1,2}(?:[./-]|月)\s?\d{1,4}日?(?:\([^)]{1,4}\))?,?\s+(?:um\s+|at\s+|à\s+|a las\s+|alle\s+|om\s+|kl\.?\s+)?\d{1,2}:\d{2}(?::\d{2})?\b.*<[^<>\s@]+@[^<>\s]+>\s*[:：]$/i;
// The header block Outlook, Exchange and many other clients put above quoted history:
// "From: …" followed within a few lines by "Sent:/Date: …" (in the common languages).
const HEADER_FROM_RE = /^(?:from|von|de|od|da|van|från|fra|lähettäjä|kimden|feladó|от|від|差出人|发件人|寄件者|보낸 사람)\s*[:：]/i;
const HEADER_SENT_LABELS = '(?:sent|date|subject|gesendet|datum|betreff|envoyé|objet|enviado|enviada|fecha|asunto|assunto|wysłano|data|temat|inviato|oggetto|verzonden|onderwerp|skickat|ämne|sendt|emne|lähetetty|aihe|odesláno|předmět|gönderildi|tarih|konu|elküldve|dátum|tárgy|отправлено|дата|тема|надіслано|送信日時|件名|发送时间|主题|寄件日期|主旨|보낸 날짜|제목)';
const HEADER_SENT_RE = new RegExp(`^${HEADER_SENT_LABELS}\\s*[:：]`, 'i');
// HTML header blocks separate their lines with <br>, which textContent flattens into one line.
const HEADER_SENT_INLINE_RE = new RegExp(`(?<![\\p{L}\\p{N}])${HEADER_SENT_LABELS}\\s*[:：]`, 'iu');
// A rule line some clients put above the header block ("________________", "-----").
const SEPARATOR_LINE_RE = /^(?:_{8,}|-{8,}|={8,})$/;

// Elements that wrap quoted history themselves (each folds on its own).
const QUOTE_WRAPPER_SELECTORS = [
  '.gmail_quote',
  '.gmail_quote_container',
  '.x_gmail_quote',
  '.moz-quote-container',
  '.yahoo_quoted',
  '[id^="yahoo_quoted"]',
  '.protonmail_quote',
  '.zmail_extra',
  '.front-blockquote',
  '#OLK_SRC_BODY_SECTION',
  '[id^="mail-editor-reference-message-container"]',
  'blockquote[type="cite"]',
];
// Elements that open quoted history: they and everything after them in the document fold.
const QUOTE_START_SELECTORS = [
  '[id$="divRplyFwdMsg"]',
  '.OutlookMessageHeader',
  '#appendonsend',
  'hr#stopSpelling',
  'hr#zwchr',
  '.moz-cite-prefix',
  '.gmail_attr',
  // Outlook desktop's header block separator (border colour differs by version/theme).
  'div[style*="border-top:solid #E1E1E1"]',
  'div[style*="border-top: solid #E1E1E1"]',
  'div[style*="border-top:solid #B5C4DF"]',
  'div[style*="border-top: solid #B5C4DF"]',
  'div[style*="border-top-color: rgb(229, 229, 229)"]',
];
const BLOCK_SELECTOR = 'p, div, blockquote, pre, table, section, article, font, center, span';

function normalizedLines(value: unknown = ''): string[] {
  return String(value)
    .replace(/\u00a0/g, ' ')
    .replace(/\r\n?/g, '\n')
    .split('\n')
    .map(line => line.replace(/[\t ]+/g, ' ').trim())
    .filter(Boolean);
}

export function startsWithForwardMarker(value: unknown = ''): boolean {
  return FORWARD_MARKER_RE.test(normalizedLines(value)[0] || '');
}

function isAttributionLine(line: string): boolean {
  if (line.length > 300) return false;
  if (DATED_SENDER_MARKER_RE.test(line)) return true;
  return WROTE_SUFFIX_RE.test(line) && /\d|@/.test(line);
}

/** Whether quoted history starts at `lines[index]` (normalized, non-empty lines). */
function replyStartsAt(lines: string[], index: number): boolean {
  const line = lines[index] || '';
  const joined = `${line} ${lines[index + 1] || ''}`;
  // A marker may be wrapped onto two lines ("On Tue, … at 10:00,\nName <x> wrote:").
  if (REPLY_MARKER_RE.test(line) || REPLY_MARKER_RE.test(joined)) return true;
  if (isAttributionLine(line) || (line.length < 200 && /\d|@/.test(line) && isAttributionLine(joined))) return true;
  if (SEPARATOR_LINE_RE.test(line)) return HEADER_FROM_RE.test(lines[index + 1] || '') && replyStartsAt(lines, index + 1);
  if (!HEADER_FROM_RE.test(line)) return false;
  return lines.slice(index + 1, index + 5).some(next => HEADER_SENT_RE.test(next)) || HEADER_SENT_INLINE_RE.test(line.slice(0, 400));
}

export function startsWithReplyMarker(value: unknown = ''): boolean {
  const lines = normalizedLines(value).slice(0, 8);
  return [0, 1, 2, 3].some(index => index < lines.length && replyStartsAt(lines, index));
}

/** Stricter than `startsWithReplyMarker`: the marker must be the element's first line. */
function beginsWithReplyMarker(value: unknown = ''): boolean {
  const lines = normalizedLines(value).slice(0, 6);
  return lines.length > 0 && replyStartsAt(lines, 0);
}

/** Labels and callback for one quote expander. */
interface QuoteToggleOptions {
  showLabel: string;
  hideLabel: string;
  onChange: (() => void) | null;
}

function uniqueTopLevel<T extends Element>(elements: T[]): T[] {
  const ordered = [...new Set(elements)].filter(Boolean);
  return ordered.filter(element => !ordered.some(other => other !== element && other.contains(element)));
}

// Node.DOCUMENT_POSITION_FOLLOWING, spelled out so this also runs where no DOM globals exist.
const DOCUMENT_POSITION_FOLLOWING = 4;
const ELEMENT_NODE = 1;
const TEXT_NODE = 3;

function documentOrder(left: Element, right: Element): number {
  if (left === right) return 0;
  return left.compareDocumentPosition(right) & DOCUMENT_POSITION_FOLLOWING ? -1 : 1;
}

function quoteWrappers(doc: Document): HTMLElement[] {
  const explicit = [...doc.querySelectorAll<HTMLElement>(QUOTE_WRAPPER_SELECTORS.join(','))];
  const markedBlockquotes = [...doc.querySelectorAll<HTMLElement>('blockquote')]
    .filter(element => startsWithReplyMarker(element.textContent));
  return [...new Set([...explicit, ...markedBlockquotes])];
}

function protectedForwardRoots(doc: Document): HTMLElement[] {
  return [...doc.querySelectorAll<HTMLElement>('.gmail_quote, .gmail_quote_container, .moz-forward-container, blockquote, div')]
    .filter(element => startsWithForwardMarker(element.textContent));
}

/** The earliest element in document order whose own text opens with a reply marker. */
function structuralStart(doc: Document, protectedSet: Set<Element>): HTMLElement | null {
  for (const element of doc.body.querySelectorAll<HTMLElement>(BLOCK_SELECTOR)) {
    if (protectedSet.has(element)) continue;
    const text = element.textContent || '';
    if (!text.trim() || startsWithForwardMarker(text)) continue;
    // Ancestors come first in document order, so the first hit is the outermost element
    // that starts exactly at the marker — never a wrapper with new content above it.
    if (beginsWithReplyMarker(text)) return element;
  }
  return null;
}

/** Pull a rule line or attribution sibling directly above `start` into the folded region. */
function widenStart(start: HTMLElement): HTMLElement {
  let current = start;
  for (let step = 0; step < 2; step += 1) {
    const previous = current.previousElementSibling as HTMLElement | null;
    if (!previous) break;
    const text = (previous.textContent || '').replace(/\u00a0/g, ' ').trim();
    const isRule = previous.tagName === 'HR' || (text !== '' && SEPARATOR_LINE_RE.test(text));
    const isAttribution = previous.matches('.gmail_attr, .moz-cite-prefix') || (text !== '' && beginsWithReplyMarker(text));
    if (!isRule && !isAttribution) break;
    current = previous;
  }
  return current;
}

/** `start` plus everything after it in the document, as foldable elements. */
function tailFrom(doc: Document, start: HTMLElement): HTMLElement[] {
  const nodes: HTMLElement[] = [start];
  let node: Element | null = start;
  while (node && node !== doc.body) {
    let sibling = node.nextSibling;
    while (sibling) {
      const next = sibling.nextSibling;
      if (sibling.nodeType === ELEMENT_NODE) {
        nodes.push(sibling as HTMLElement);
      } else if (sibling.nodeType === TEXT_NODE && (sibling.textContent || '').trim()) {
        // A bare text node can't carry the collapse class: wrap it.
        const span = doc.createElement('span');
        sibling.parentNode?.insertBefore(span, sibling);
        span.append(sibling);
        nodes.push(span);
      }
      sibling = next;
    }
    node = node.parentElement;
  }
  return nodes;
}

function candidateGroups(doc: Document, protectedForwards: HTMLElement[]): HTMLElement[][] {
  const protectedSet = new Set<Element>(protectedForwards);
  // A forwarded envelope remains visible, but quote candidates nested inside it are
  // still eligible. Filtering forward roots before top-level de-duplication is what
  // preserves Gmail's hierarchical behaviour for forwards containing old replies.
  const wrappers = uniqueTopLevel(quoteWrappers(doc)
    .filter(element => !protectedSet.has(element))
    .filter(element => !startsWithForwardMarker(element.textContent)));
  // One expander per historical region: the first start marker (an Outlook header block,
  // an attribution line, a separator) and everything after it collapse together, at any
  // nesting depth — Outlook and Word wrap the whole body, history included, in one div.
  const starts = [
    ...[...doc.querySelectorAll<HTMLElement>(QUOTE_START_SELECTORS.join(','))]
      .filter(element => !protectedSet.has(element) && !startsWithForwardMarker(element.textContent)),
    structuralStart(doc, protectedSet),
  ].filter((element): element is HTMLElement => element !== null)
    .filter(element => !wrappers.some(wrapper => wrapper.contains(element)))
    .sort(documentOrder);
  const start = starts[0];
  if (!start) return wrappers.map(wrapper => [widenStart(wrapper)]);
  const tail = tailFrom(doc, widenStart(start));
  const inTail = (element: Element) => tail.some(node => node === element || node.contains(element));
  const groups = wrappers.filter(wrapper => !inTail(wrapper)).map(wrapper => [widenStart(wrapper)]);
  groups.push(tail);
  return groups;
}

function hasContentBefore(doc: Document, element: Element): boolean {
  const range = doc.createRange();
  range.setStart(doc.body, 0);
  range.setEndBefore(element);
  const before = range.cloneContents();
  if ((before.textContent || '').replace(/\u00a0/g, ' ').trim()) return true;
  return Boolean(before.querySelector('img, video, table, hr'));
}

function createToggle(doc: Document, id: string, showLabel: string, hideLabel: string, onChange: (() => void) | null): HTMLElement {
  const button = doc.createElement('button');
  button.type = 'button';
  button.className = 'mailflow-quote-toggle';
  button.dataset.mailflowQuoteToggle = id;
  button.setAttribute('aria-expanded', 'false');
  button.textContent = showLabel;
  button.addEventListener('click', event => {
    event.preventDefault();
    event.stopPropagation();
    const expanded = button.getAttribute('aria-expanded') === 'true';
    const nextExpanded = !expanded;
    for (const node of doc.querySelectorAll(`[data-mailflow-quote-group="${id}"]`)) {
      node.classList.toggle('mailflow-quote-collapsed', !nextExpanded);
    }
    button.setAttribute('aria-expanded', String(nextExpanded));
    button.textContent = nextExpanded ? hideLabel : showLabel;
    if (onChange !== null) onChange();
  });
  return button;
}

function installGroup(doc: Document, elements: HTMLElement[], index: number, options: QuoteToggleOptions): void {
  if (!elements.length) return;
  const id = `mailflow-quote-${index}`;
  // One toggle before the first element; all elements share the same group ID so
  // they collapse/expand together. This produces a single "Show quoted text" button
  // for the entire historical region (one expander per region).
  elements.forEach(element => {
    element.dataset.mailflowQuoteGroup = id;
    element.classList.add('mailflow-quote-collapsed');
  });
  elements[0].parentNode?.insertBefore(createToggle(doc, id, options.showLabel, options.hideLabel, options.onChange), elements[0]);
}

export function plainTextBoundary(text: unknown): number {
  const lines = String(text).replace(/\r\n?/g, '\n').split('\n');
  let offset = 0;
  for (let index = 0; index < lines.length; index += 1) {
    const line = lines[index].replace(/\u00a0/g, ' ').trim();
    const following = lines.slice(index, index + 6).map(candidate => candidate.replace(/\u00a0/g, ' ').replace(/[\t ]+/g, ' ').trim()).filter(Boolean);
    if (line && !startsWithForwardMarker(line) && replyStartsAt(following, 0)) return offset;
    if (/^>/.test(line)) {
      const quotedTail = lines.slice(index).filter(candidate => candidate.trim()).every(candidate => /^\s*>/.test(candidate));
      const quotedCount = lines.slice(index).filter(candidate => /^\s*>/.test(candidate)).length;
      if (quotedTail && quotedCount >= 2) return offset;
    }
    offset += lines[index].length + 1;
  }
  return -1;
}

function installPlainTextGroup(doc: Document, pre: Element, options: QuoteToggleOptions): boolean {
  const text = pre.textContent || '';
  const boundary = plainTextBoundary(text);
  if (boundary < 0) return false;
  const visible = text.slice(0, boundary);
  // Nothing written above the quoted history: the message *is* that history (a forward), so
  // folding it would leave only the toggle.
  if (!visible.trim()) return false;
  const quoted = text.slice(boundary);
  const id = 'mailflow-quote-plain';
  pre.textContent = '';
  pre.append(doc.createTextNode(visible));
  const quote = doc.createElement('span');
  quote.dataset.mailflowQuoteGroup = id;
  quote.className = 'mailflow-quote-collapsed';
  quote.textContent = quoted;
  pre.append(createToggle(doc, id, options.showLabel, options.hideLabel, options.onChange), quote);
  return true;
}

export function installMessageQuoteFolding(doc: Document, {
  showLabel = 'Show quoted text',
  hideLabel = 'Hide quoted text',
  onChange = null,
}: { showLabel?: string; hideLabel?: string; onChange?: (() => void) | null } = {}) {
  const plainText = doc.querySelector('[data-mailflow-plain-text="true"]');
  if (plainText) {
    const installed = installPlainTextGroup(doc, plainText, { showLabel, hideLabel, onChange });
    return { count: installed ? 1 : 0, protectedForwardCount: startsWithForwardMarker(plainText.textContent) ? 1 : 0 };
  }

  const protectedForwards = protectedForwardRoots(doc);
  protectedForwards.forEach(root => { root.dataset.mailflowForwardRoot = 'true'; });
  const candidates = candidateGroups(doc, protectedForwards);
  // Nothing written above the first quoted region: the message *is* that history (a forward,
  // or a mail sent on unchanged), so folding it would leave only the toggle.
  const first = candidates.map(group => group[0]).sort(documentOrder)[0];
  const groups = first && hasContentBefore(doc, first) ? candidates : [];
  groups.forEach((elements, index) => installGroup(doc, elements, index, { showLabel, hideLabel, onChange }));
  return { count: groups.length, protectedForwardCount: protectedForwards.length };
}
