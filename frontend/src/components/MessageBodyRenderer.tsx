import { useEffect, useRef, useMemo } from 'react';
export { EMAIL_SANITIZE_POLICY, sanitizeMessageHtml, emailCsp, EMAIL_BASE_TAG, buildSrcDoc } from './messageBodySecurity.ts';
import { sanitizeMessageHtml, buildSrcDoc, escapeMessageText } from './messageBodySecurity.ts';
import { installMessageQuoteFolding } from './messageQuoteFolding.ts';
import { scheduleInitialLayoutReady } from './messageBodyLayout.ts';
import { useStore } from '../store/index.ts';
import { getEmailSurface } from '../themes.ts';

/**
 * SafeMessageFrame — shared production HTML body renderer using a sandboxed iframe.
 *
 * Both single-message and conversation reader modes use this component so they have an
 * IDENTICAL security model:
 *   - sandboxed iframe (allow-same-origin only — NO allow-scripts)
 *   - CSP via meta tag (default-src 'none', no remote resources unless explicitly enabled)
 *   - DOMPurify sanitization with EMAIL_SANITIZE_POLICY
 *   - <base target="_blank" rel="noopener noreferrer">
 *   - remote-image blocking (default), with opt-in for allow-listed senders
 *   - CID references preserved (backend resolves to attachment content)
 *   - auto-height sizing to content
 *
 * The iframe has NO allow-scripts, so even if an attacker injects <script>,
 * event handlers, javascript: URIs, or SVG tricks, they cannot execute.
 * CSS external URLs are blocked by CSP + regex sanitization.
 * iframe/object/embed/form are stripped by DOMPurify FORBID_TAGS.
 */
/** The props of the sandboxed message-body frame. */
interface MessageBodyRendererProps {
  html?: string;
  text?: string;
  remoteImages?: boolean;
  quoteFolding?: boolean;
  onQuoteDetected?: ((detected: boolean) => void) | null;
  onHeightChange?: ((height: number) => void) | null;
  onInitialLayoutReady?: ((height: number) => void) | null;
  iframeRef?: { current: HTMLIFrameElement | null } | null;
  onLoad?: (() => void) | null;
  title?: string;
  showQuotedTextLabel?: string;
  hideQuotedTextLabel?: string;
  style?: React.CSSProperties | null;
  onContextMenu?: ((position: { x: number; y: number; selectedText: string }) => void) | null;
  onOpenLink?: ((url: string) => void) | null;
}


export default function MessageBodyRenderer({ html = '', text = '', remoteImages = false, quoteFolding = true, onQuoteDetected = null, onHeightChange = null, onInitialLayoutReady = null, iframeRef: externalIframeRef = null, onLoad = null, title = 'Message body', showQuotedTextLabel = 'Show quoted text', hideQuotedTextLabel = 'Hide quoted text', style: frameStyle = null, onContextMenu = null, onOpenLink = null }: MessageBodyRendererProps) {
  const internalIframeRef = useRef<HTMLIFrameElement | null>(null);
  const iframeRef = externalIframeRef || internalIframeRef;

  // The active theme, so the frame's own document can declare a matching surface.
  // Subscribing here (rather than at each call site) keeps message bodies, calendar
  // descriptions and every future embed on the same contract.
  const theme = useStore((state: { theme: string }) => state.theme);

  const srcDoc = useMemo(() => {
    // The surface decides the canvas; its tone also tells the sanitiser whether the
    // message has to be adapted to a dark canvas before it is written into the frame.
    const surface = getEmailSurface(theme);
    const content = html
      ? sanitizeMessageHtml(html, { remoteImages, tone: surface?.tone })
      : `<pre data-mailflow-plain-text="true">${escapeMessageText(text)}</pre>`;
    return buildSrcDoc(content, { remoteImages, surface });
  }, [html, text, remoteImages, theme]);

  // Auto-height: measure the iframe content and set the iframe height
  // so no internal scrollbar appears (same approach as MessagePane).
  useEffect(() => {
    const iframe = iframeRef.current;
    if (!iframe || !srcDoc) return;
    // Keep the new body invisible (it still lays out and measures) until quote folding has
    // run, so quoted history is never painted expanded and then collapsed — no flicker.
    // A fallback reveals it should the frame never report a load.
    iframe.style.visibility = 'hidden';
    const revealFallback = setTimeout(() => { iframe.style.visibility = ''; }, 2000);

    let initialLayoutReported = false;
    let cancelInitialLayout: (() => void) | null = null;
    const measure = () => {
      try {
        const doc = iframe.contentDocument;
        if (!doc?.body) return;
        // Do not include documentElement.scrollHeight: once the iframe has grown, that
        // value is at least the old iframe viewport height and prevents a later quote
        // collapse from shrinking the frame. Resetting to a natural viewport before
        // reading body.scrollHeight makes H3 reflect the currently visible DOM.
        iframe.style.height = '0px';
        const contentHeight = Math.max(300, doc.body.scrollHeight || 0);
        iframe.style.height = contentHeight + 'px';
        onHeightChange?.(contentHeight);
        if (!initialLayoutReported) {
          // The iframe height participates in its parent reader's scroll range on
          // the following paint. Wait an additional frame so the parent card's
          // scrollHeight reflects that committed height before final alignment.
          cancelInitialLayout?.();
          cancelInitialLayout = scheduleInitialLayoutReady(() => {
            initialLayoutReported = true;
            cancelInitialLayout = null;
            onInitialLayoutReady?.(contentHeight);
          });
        }
      } catch {
        // Cross-origin or not yet loaded — leave default height.
      }
    };
    const install = () => {
      const doc = iframe.contentDocument;
      if (!doc) return;
      const expanded = new Map<HTMLElement, { overflowY: string; overflowYPriority: string; height: string; heightPriority: string; maxHeight: string; maxHeightPriority: string }>();
      let quoteResult = { count: 0 };
      const expandScrollContainers = () => {
        const elements = [...doc.querySelectorAll<HTMLElement>('*')].reverse();
        for (const el of elements) {
          const style = doc.defaultView?.getComputedStyle(el);
          if (!style) continue;
          const isScroll = (style.overflowY === 'auto' || style.overflowY === 'scroll')
            && el.scrollHeight > el.clientHeight + 2;
          if (!isScroll) continue;
          if (!expanded.has(el)) {
            expanded.set(el, {
              overflowY: el.style.getPropertyValue('overflow-y'),
              overflowYPriority: el.style.getPropertyPriority('overflow-y'),
              height: el.style.getPropertyValue('height'),
              heightPriority: el.style.getPropertyPriority('height'),
              maxHeight: el.style.getPropertyValue('max-height'),
              maxHeightPriority: el.style.getPropertyPriority('max-height'),
            });
          }
          el.style.setProperty('overflow-y', 'hidden', 'important');
          el.style.setProperty('max-height', 'none', 'important');
          el.style.setProperty('height', `${el.scrollHeight}px`, 'important');
        }
      };
      const restoreScrollContainers = () => {
        for (const [el, previous] of expanded) {
          for (const [name, value, priority] of [
            ['overflow-y', previous.overflowY, previous.overflowYPriority],
            ['height', previous.height, previous.heightPriority],
            ['max-height', previous.maxHeight, previous.maxHeightPriority],
          ]) {
            if (value) el.style.setProperty(name, value, priority);
            else el.style.removeProperty(name);
          }
        }
        expanded.clear();
      };
      const measureExpanded = () => {
        // Restore scroll containers to their natural state FIRST so that after a
        // quote collapse the iframe SHRINKS. Without this, the height overrides
        // applied during expand persist and the card retains ~2900px of empty space.
        restoreScrollContainers();
        expandScrollContainers();
        measure();
      };
      if (quoteFolding) {
        quoteResult = installMessageQuoteFolding(doc, {
          showLabel: showQuotedTextLabel,
          hideLabel: hideQuotedTextLabel,
          onChange: measureExpanded,
        });
      }
      onQuoteDetected?.(quoteResult.count > 0);
      const images = [...doc.images];
      const onDocumentClick = (event: MouseEvent) => {
        // Elements from an iframe belong to its realm, so they are not instances of
        // the parent document's Element/HTMLAnchorElement constructors.
        const FrameElement = doc.defaultView?.Element;
        if (!FrameElement || !(event.target instanceof FrameElement)) return;
        const anchor = event.target.closest('a[href]');
        if (!anchor) return;
        const raw = anchor.getAttribute('href') || '';
        let url: URL;
        try { url = new URL(raw, window.location.href); } catch { event.preventDefault(); return; }
        if (url.protocol === 'http:' || url.protocol === 'https:' || url.protocol === 'mailto:') {
          event.preventDefault();
          // The body is sandboxed, so opening from its own browsing context can be
          // blocked. Hand the user gesture to the top-level Inboxora document instead.
          if (onOpenLink) onOpenLink(url.href);
          else window.open(url.href, '_blank', 'noopener,noreferrer');
        } else event.preventDefault();
      };
      const onDocumentContextMenu = (event: MouseEvent) => {
        if (onContextMenu === null) return;
        // Long-press selection and the platform copy menu are more useful on touch
        // devices than a desktop action menu. Keep the browser's native affordance.
        if (doc.defaultView?.matchMedia('(pointer: coarse)').matches) return;
        const selectionObject = doc.getSelection();
        const selection = selectionObject === null ? '' : selectionObject.toString();
        // Always suppress the browser menu on desktop, including on links and images.
        // The same Inboxora menu is then available wherever the user right-clicks.
        event.preventDefault();
        const rect = iframe.getBoundingClientRect();
        onContextMenu({ x: rect.left + event.clientX, y: rect.top + event.clientY, selectedText: selection });
      };
      doc.addEventListener('click', onDocumentClick);
      doc.addEventListener('contextmenu', onDocumentContextMenu);
      for (const image of images) {
        image.addEventListener('load', measureExpanded);
        image.addEventListener('error', measureExpanded);
      }
      let observer = null;
      if (typeof ResizeObserver !== 'undefined' && doc.body) {
        observer = new ResizeObserver(measureExpanded);
        observer.observe(doc.body);
      }
      measureExpanded();
      // Only the message document itself may reveal the frame — not the initial about:blank.
      if (doc.URL === 'about:srcdoc') {
        clearTimeout(revealFallback);
        iframe.style.visibility = '';
      }
      onLoad?.();
      return () => {
        doc.removeEventListener('click', onDocumentClick);
        doc.removeEventListener('contextmenu', onDocumentContextMenu);
        for (const image of images) {
          image.removeEventListener('load', measureExpanded);
          image.removeEventListener('error', measureExpanded);
        }
        observer?.disconnect();
        restoreScrollContainers();
      };
    };
    let cleanup: (() => void) | null = null;
    let installedDoc: Document | null = null;
    const onLoaded = () => {
      const doc = iframe.contentDocument;
      // Folding and listeners are installed once per document; image loads re-measure on
      // their own, so the later `load` event must not fold a second time.
      if (doc && doc === installedDoc) return;
      installedDoc = doc;
      cleanup?.(); cleanup = install() || null;
    };
    // Install as soon as the message document is parsed — before its images finish loading
    // and before that content is first painted — instead of waiting for `load`, which a slow
    // image can delay for seconds while the quote shows expanded.
    let parsePoll = 0;
    const pollParsed = () => {
      const doc = iframe.contentDocument;
      if (doc && doc.URL === 'about:srcdoc' && doc.readyState !== 'loading' && doc.body) { onLoaded(); return; }
      parsePoll = requestAnimationFrame(pollParsed);
    };
    parsePoll = requestAnimationFrame(pollParsed);
    iframe.addEventListener('load', onLoaded);
    if (iframe.contentDocument?.readyState === 'complete') onLoaded();
    return () => {
      cancelAnimationFrame(parsePoll);
      cleanup?.();
      cancelInitialLayout?.();
      clearTimeout(revealFallback);
      iframe.removeEventListener('load', onLoaded);
    };
  }, [srcDoc, remoteImages, quoteFolding, showQuotedTextLabel, hideQuotedTextLabel, onQuoteDetected, onHeightChange, onInitialLayoutReady, onLoad, onContextMenu, onOpenLink, iframeRef]);


  return (
    <iframe
      ref={iframeRef}
      srcDoc={srcDoc}
      title={title}
      sandbox="allow-same-origin"
      loading="lazy"
      style={{
        width: '100%',
        height: '300px',
        border: 'none',
        display: 'block',
        background: 'transparent',
        ...frameStyle,
      }}
      // Prevent the iframe from being a drag/drop target for external content.
      onDragStart={ (e: React.DragEvent) => e.preventDefault()}
    />
  );
}
