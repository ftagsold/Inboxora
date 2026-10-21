/**
 * Parse untrusted HTML into an inert document for reading its text/structure. Assigning
 * innerHTML on an element of the live document — even a detached one — fetches resources and
 * fires handlers such as <img onerror>; a DOMParser document never loads or executes anything.
 */
export function parseInertHtml(html: string): HTMLElement {
  return new DOMParser().parseFromString(html, 'text/html').body;
}
