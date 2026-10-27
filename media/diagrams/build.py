#!/usr/bin/env python3
"""Generates the README diagrams in this folder as standalone SVG files.

Run from anywhere: python3 media/diagrams/build.py
"""

from pathlib import Path
from xml.sax.saxutils import escape

OUT = Path(__file__).resolve().parent
FONT = "-apple-system, BlinkMacSystemFont, 'Segoe UI', Inter, Helvetica, Arial, sans-serif"
MONO = "ui-monospace, SFMono-Regular, Menlo, Consolas, monospace"

INK = "#0f172a"
MUTED = "#475569"
FAINT = "#94a3b8"
LINE = "#64748b"

# One hue per place data lives.
BROWSER = {"fill": "#eff6ff", "edge": "#93c5fd", "ink": "#1d4ed8", "card": "#ffffff"}
SERVER = {"fill": "#f5f3ff", "edge": "#c4b5fd", "ink": "#6d28d9", "card": "#ffffff"}
REDIS = {"fill": "#fef2f2", "edge": "#fca5a5", "ink": "#b91c1c", "card": "#ffffff"}
PG = {"fill": "#f0f9ff", "edge": "#7dd3fc", "ink": "#0369a1", "card": "#ffffff"}
IMAP = {"fill": "#ecfdf5", "edge": "#6ee7b7", "ink": "#047857", "card": "#ffffff"}
USER = {"fill": "#f8fafc", "edge": "#cbd5e1", "ink": "#334155", "card": "#ffffff"}


def svg(width, height, body, label):
    return f"""<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}" role="img" aria-label="{escape(label)}">
<defs>
  <marker id="arrow" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse">
    <path d="M0,0 L10,5 L0,10 z" fill="{LINE}"/>
  </marker>
  <marker id="arrow-green" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse">
    <path d="M0,0 L10,5 L0,10 z" fill="{IMAP['ink']}"/>
  </marker>
  <filter id="shadow" x="-10%" y="-10%" width="120%" height="140%">
    <feDropShadow dx="0" dy="1.5" stdDeviation="2" flood-color="#0f172a" flood-opacity="0.08"/>
  </filter>
</defs>
<rect x="0.5" y="0.5" width="{width - 1}" height="{height - 1}" rx="16" fill="#ffffff" stroke="#e2e8f0"/>
<g font-family="{FONT}">
{body}
</g>
</svg>
"""


def text(x, y, s, size=13, color=INK, weight=400, anchor="start", family=None, italic=False):
    fam = f' font-family="{family}"' if family else ""
    style = ' font-style="italic"' if italic else ""
    return (f'<text x="{x}" y="{y}" font-size="{size}" fill="{color}" font-weight="{weight}" '
            f'text-anchor="{anchor}"{fam}{style}>{escape(s)}</text>')


def zone(x, y, w, h, title, c):
    return (f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="14" fill="{c["fill"]}" stroke="{c["edge"]}" stroke-dasharray="5 4"/>'
            + text(x + 18, y + 28, title.upper(), size=11.5, color=c["ink"], weight=700)
            .replace("<text ", '<text letter-spacing="1.2" ', 1))


def pill(x, y, s, c, anchor="start"):
    w = 7.0 * len(s) + 18
    if anchor == "middle":
        x -= w / 2
    return (f'<rect x="{x}" y="{y}" width="{w}" height="22" rx="11" fill="{c["fill"]}" stroke="{c["edge"]}"/>'
            + text(x + w / 2, y + 15, s, size=11.5, color=c["ink"], weight=600, anchor="middle"))


def card(x, y, w, h, title, lines, c, badge=None, icon=None):
    out = [f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="12" fill="{c["card"]}" stroke="{c["edge"]}" filter="url(#shadow)"/>',
           f'<rect x="{x}" y="{y}" width="6" height="{h}" rx="3" fill="{c["ink"]}"/>']
    tx = x + 22
    if icon:
        out.append(icon(tx, y + 18, c["ink"]))
        tx += 28
    out.append(text(tx, y + 33, title, size=15, weight=700))
    for i, line in enumerate(lines):
        out.append(text(x + 22, y + 60 + i * 20, line, size=13, color=MUTED))
    if badge:
        out.append(pill(x + 22, y + h - 34, badge, c))
    return "\n".join(out)


def db_icon(x, y, color):
    return (f'<g fill="none" stroke="{color}" stroke-width="1.8">'
            f'<ellipse cx="{x + 9}" cy="{y + 4}" rx="9" ry="3.5"/>'
            f'<path d="M{x} {y + 4} v12 c0 2 4 3.5 9 3.5 s9 -1.5 9 -3.5 v-12"/>'
            f'<path d="M{x} {y + 10} c0 2 4 3.5 9 3.5 s9 -1.5 9 -3.5"/></g>')


def chip_icon(x, y, color):
    return (f'<g fill="none" stroke="{color}" stroke-width="1.8">'
            f'<rect x="{x + 1}" y="{y}" width="16" height="16" rx="3"/>'
            f'<path d="M{x + 5} {y + 5} h8 M{x + 5} {y + 9} h8 M{x + 5} {y + 13} h5"/></g>')


def mail_icon(x, y, color):
    return (f'<g fill="none" stroke="{color}" stroke-width="1.8">'
            f'<rect x="{x}" y="{y + 1}" width="18" height="14" rx="2.5"/>'
            f'<path d="M{x + 1} {y + 3} l8 6 l8 -6"/></g>')


def arrow(x1, y1, x2, y2, label=None, sub=None, dashed=False, color=LINE, marker="arrow", label_dy=-10):
    dash = ' stroke-dasharray="5 4"' if dashed else ""
    out = [f'<line x1="{x1}" y1="{y1}" x2="{x2}" y2="{y2}" stroke="{color}" stroke-width="1.8"{dash} marker-end="url(#{marker})"/>']
    mx, my = (x1 + x2) / 2, (y1 + y2) / 2
    if label:
        out.append(text(mx, my + label_dy, label, size=12, color=MUTED, weight=600, anchor="middle"))
    if sub:
        out.append(text(mx, my + 20, sub, size=11.5, color=FAINT, anchor="middle"))
    return "\n".join(out)


# ── 1. Cache layers ────────────────────────────────────────────────────────────

def cache_layers():
    W, H = 1100, 420
    b = []
    b.append(zone(20, 20, 520, 380, "Browser · per user", BROWSER))
    b.append(zone(580, 20, 250, 380, "Inboxora server", SERVER))
    b.append(zone(870, 20, 210, 380, "Mail provider", IMAP))

    b.append(card(40, 70, 210, 150, "Memory", ["12 recent list views", "rows ready to paint"], BROWSER,
                  badge="until tab closes", icon=chip_icon))
    b.append(card(310, 70, 210, 150, "IndexedDB", ["display fields", "2,000 bodies", ], BROWSER,
                  badge="bodies 7 days", icon=db_icon))
    b.append(card(600, 70, 210, 150, "Redis", ["display fields of every", "message, plus bodies"], REDIS,
                  badge="memory-only", icon=db_icon))
    b.append(card(600, 250, 210, 130, "PostgreSQL", ["header index only:", "UIDs, flags, threading"], PG,
                  badge="permanent", icon=db_icon))
    b.append(card(890, 70, 170, 150, "IMAP", ["the mail itself", "source of truth"], IMAP,
                  badge="permanent", icon=mail_icon))

    b.append(arrow(254, 145, 306, 145, "miss"))
    b.append(arrow(524, 145, 596, 145, "miss"))
    b.append(arrow(814, 145, 886, 145, "miss"))
    b.append(arrow(705, 248, 705, 224, dashed=True))
    b.append(text(716, 241, "which ids exist", size=11.5, color=FAINT))

    b.append(text(40, 262, "Every click is answered from the", size=13, color=MUTED))
    b.append(text(40, 282, "nearest layer that has the data.", size=13, color=MUTED))
    b.append(text(40, 316, "Subjects, senders and bodies never", size=13, color=MUTED))
    b.append(text(40, 336, "change, so cached copies never go stale.", size=13, color=MUTED))
    b.append(text(40, 370, "Cleared on sign-out.", size=13, color=FAINT, italic=True))

    b.append(text(890, 262, "Touched only on a", size=13, color=MUTED))
    b.append(text(890, 282, "miss, with one fetch", size=13, color=MUTED))
    b.append(text(890, 302, "per folder.", size=13, color=MUTED))
    return svg(W, H, "\n".join(b), "Three cache layers in front of the IMAP server")


# ── 2. Opening a folder ────────────────────────────────────────────────────────

def open_folder():
    lanes = [("You", USER, 90), ("Browser", BROWSER, 290), ("Inboxora server", SERVER, 530),
             ("Redis", REDIS, 760), ("IMAP", IMAP, 960)]
    x = {name: lx for name, _, lx in lanes}
    W, top, row = 1060, 96, 46
    steps = [
        ("You", "Browser", "Click a folder", False, None),
        ("Browser", "You", "Cached rows, same frame", True, "instant"),
        ("Browser", "Inboxora server", "GET changes since last seq", False, None),
        ("Inboxora server", "Redis", "Display fields for new ids", False, None),
        ("Redis", "Inboxora server", "Hits", True, None),
        ("Inboxora server", "IMAP", "UID FETCH, one per folder", False, "miss"),
        ("IMAP", "Inboxora server", "Headers and snippet", True, "miss"),
        ("Inboxora server", "Redis", "Write through", False, "miss"),
        ("Inboxora server", "Browser", "Changes only", True, None),
        ("Browser", "You", "Merged into the list", True, None),
    ]
    H = top + row * len(steps) + 40
    b = []
    for name, c, lx in lanes:
        b.append(f'<line x1="{lx}" y1="62" x2="{lx}" y2="{H - 24}" stroke="{c["edge"]}" stroke-width="2" stroke-dasharray="2 5"/>')
        w = 7.6 * len(name) + 36
        b.append(f'<rect x="{lx - w / 2}" y="22" width="{w}" height="36" rx="18" fill="{c["fill"]}" stroke="{c["edge"]}"/>')
        b.append(text(lx, 45, name, size=14, color=c["ink"], weight=700, anchor="middle"))

    miss_rows = [i for i, s in enumerate(steps) if s[4] == "miss"]
    y0 = top + row * miss_rows[0] - 26
    y1 = top + row * miss_rows[-1] + 34
    b.append(f'<rect x="{x["Inboxora server"] - 30}" y="{y0}" width="{x["IMAP"] - x["Inboxora server"] + 80}" height="{y1 - y0}" '
             f'rx="10" fill="{IMAP["fill"]}" fill-opacity="0.6" stroke="{IMAP["edge"]}" stroke-dasharray="5 4"/>')
    b.append(text(x["IMAP"] + 38, y1 - 10, "ONLY ON A CACHE MISS", size=10.5, color=IMAP["ink"], weight=700, anchor="end")
             .replace("<text ", '<text letter-spacing="1" ', 1))

    for i, (src, dst, label, back, tag) in enumerate(steps):
        y = top + row * i
        x1, x2 = x[src], x[dst]
        d = 1 if x2 > x1 else -1
        green = tag == "instant"
        color = IMAP["ink"] if green else LINE
        marker = "arrow-green" if green else "arrow"
        dash = ' stroke-dasharray="6 4"' if back else ""
        b.append(f'<line x1="{x1 + d * 6}" y1="{y}" x2="{x2 - d * 8}" y2="{y}" stroke="{color}" stroke-width="{2.4 if green else 1.8}"{dash} marker-end="url(#{marker})"/>')
        lx = (x1 + x2) / 2
        b.append(text(lx, y - 9, label, size=13, color=IMAP["ink"] if green else INK, weight=700 if green else 500, anchor="middle"))
        b.append(f'<circle cx="{x1}" cy="{y}" r="11" fill="{INK if not green else IMAP["ink"]}"/>')
        b.append(text(x1, y + 4, str(i + 1), size=11, color="#ffffff", weight=700, anchor="middle"))
        if green:
            b.append(pill((x1 + x2) / 2, y + 8, "no network wait", IMAP, anchor="middle"))
    return svg(W, H, "\n".join(b), "Opening a folder: cached rows render at once, then only changes are fetched")


# ── 3. Prefetch ────────────────────────────────────────────────────────────────

def trigger_card(x, y, w, h, when, what, c):
    out = [f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="12" fill="#ffffff" stroke="{c["edge"]}" filter="url(#shadow)"/>',
           f'<path d="M{x} {y + 12} a12 12 0 0 1 12 -12 h{w - 24} a12 12 0 0 1 12 12 v26 h-{w} z" fill="{c["fill"]}"/>',
           f'<line x1="{x}" y1="{y + 38}" x2="{x + w}" y2="{y + 38}" stroke="{c["edge"]}"/>',
           text(x + 16, y + 25, "WHEN", size=10.5, color=c["ink"], weight=700).replace("<text ", '<text letter-spacing="1" ', 1),
           text(x + 60, y + 25, when, size=13.5, color=c["ink"], weight=700)]
    for i, line in enumerate(what):
        out.append(text(x + 16, y + 64 + i * 20, line, size=13.5, color=INK if i == 0 else MUTED, weight=600 if i == 0 else 400))
    return "\n".join(out)


def prefetch():
    W, H = 1040, 380
    b = [zone(20, 20, 1000, 160, "Browser · while you work", BROWSER),
         zone(20, 200, 1000, 160, "Server · in the background", SERVER)]
    cw, gap = 225, 20
    for i, (when, what) in enumerate([
        ("app is idle", ["Load every INBOX", "first page of each account"]),
        ("you hover a folder", ["Load its first page", "before you click"]),
        ("a list loads", ["Load 8 unread bodies", "and the next page"]),
        ("a thread opens", ["Load bodies 3 at a time", "outwards from the selected"]),
    ]):
        b.append(trigger_card(40 + i * (cw + gap), 62, cw, 100, when, what, BROWSER))
    cw = 306
    for i, (when, what) in enumerate([
        ("the server starts", ["Preload subject, sender, snippet", "of every folder into Redis"]),
        ("a sync runs", ["Fetch display fields", "for new ids only"]),
        ("mail is deleted", ["Drop its cached entries", "plus a daily sweep"]),
    ]):
        b.append(trigger_card(40 + i * (cw + 20), 242, cw, 100, when, what, SERVER))
    return svg(W, H, "\n".join(b), "What the browser and the server load ahead of time")


# ── 4. IMAP connection budget ──────────────────────────────────────────────────

def imap_budget():
    W, H = 1040, 380
    b = []
    # Incoming requests.
    b.append(card(30, 110, 220, 140, "Requests", ["from the interface,", "sync and preload"], USER, badge="queue by priority", icon=chip_icon))
    b.append(arrow(254, 180, 316, 180))
    # The budget gate with ten slots.
    b.append(f'<rect x="320" y="80" width="250" height="200" rx="14" fill="{SERVER["fill"]}" stroke="{SERVER["edge"]}" filter="url(#shadow)"/>')
    b.append(text(445, 114, "Connection budget", size=15, weight=700, anchor="middle"))
    b.append(text(445, 136, "per account, default 10", size=13, color=MUTED, anchor="middle"))
    for i in range(10):
        cx = 345 + (i % 5) * 42
        cy = 156 + (i // 5) * 42
        used = i < 7
        b.append(f'<rect x="{cx}" y="{cy}" width="32" height="32" rx="7" fill="{SERVER["ink"] if used else "#ffffff"}" '
                 f'stroke="{SERVER["ink"]}" stroke-opacity="{1 if used else 0.45}"/>')
    b.append(text(445, 262, "every socket holds one slot", size=12, color=FAINT, anchor="middle"))
    b.append(arrow(574, 180, 636, 180))
    # Priority ladder.
    tiers = ["Interface", "INBOX", "Archive, Sent, Trash, Drafts, Junk", "Other folders", "Background preload"]
    shades = ["#4c1d95", "#6d28d9", "#8b5cf6", "#a78bfa", "#c4b5fd"]
    b.append(text(640, 46, "SERVED IN THIS ORDER", size=11.5, color=SERVER["ink"], weight=700).replace("<text ", '<text letter-spacing="1.2" ', 1))
    for i, (tier, shade) in enumerate(zip(tiers, shades)):
        y = 62 + i * 54
        w = 370 - i * 30
        b.append(f'<rect x="640" y="{y}" width="{w}" height="42" rx="10" fill="#ffffff" stroke="{SERVER["edge"]}" filter="url(#shadow)"/>')
        b.append(f'<rect x="640" y="{y}" width="{w}" height="42" rx="10" fill="{shade}" fill-opacity="0.12"/>')
        b.append(f'<circle cx="662" cy="{y + 21}" r="12" fill="{shade}"/>')
        b.append(text(662, y + 25.5, str(i + 1), size=12, color="#ffffff", weight=700, anchor="middle"))
        b.append(text(686, y + 26, tier, size=14, weight=600))
    b.append(text(30, 330, "A refused or early-dropped connection lowers the limit by one:", size=13, color=MUTED))
    b.append(text(30, 352, "at most every 30 s, never below 2. The learned limit survives restarts.", size=13, color=MUTED))
    return svg(W, H, "\n".join(b), "IMAP connection budget and request priority")


for name, build in [("cache-layers", cache_layers), ("open-folder", open_folder),
                    ("prefetch", prefetch), ("imap-budget", imap_budget)]:
    (OUT / f"{name}.svg").write_text(build())
    print(f"wrote {name}.svg")
