#!/usr/bin/env python3
"""Validate a writing-pipeline 正文.html — the WeChat-compliant inline-styled HTML
emitted by article-formatter (shape only; visual aesthetics is human review's
job). HTML_OK / HTML_INVALID:N + return 0/1.

HTML validity is the most deterministically checkable step in the chain, so
this gate is the strictest. It enforces the formatter's own post-processing
rules: only `<section>` (no `<div>`), no `<h1>`, no `<style>`/`class=` (all
inline), no leftover `<!-- META -->` head, no un-converted markdown, balanced
`<section>` tags, and every `<img>` carries a non-empty src. Pure structure.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

# Lines that indicate un-converted Markdown leaked into the HTML output.
MD_LINE_RES = [
    re.compile(r"^\s{0,3}#{1,6}\s"),   # ATX headings
    re.compile(r"^\s*[-*+]\s+"),       # bullet list
    re.compile(r"^\s*\d+[.、)]\s"),     # ordered list
    re.compile(r"^\s{0,3}>\s?"),       # blockquote
]
# Anywhere-in-text markdown markers that should never survive formatting.
MD_ANYWHERE_RES = [
    re.compile(r"\*\*"),               # bold
    re.compile(r"`"),                  # inline code / code fence
    re.compile(r"\]\("),               # link target
]
SECTION_OPEN_RE = re.compile(r"<section\b", re.IGNORECASE)
SECTION_CLOSE_RE = re.compile(r"</section\s*>", re.IGNORECASE)
IMG_RE = re.compile(r"<img\b[^>]*>", re.IGNORECASE)
SRC_RE = re.compile(r"""src\s*=\s*["']([^"']*)["']""", re.IGNORECASE)


def validate(html: str) -> tuple[list[str], list[str]]:
    errors: list[str] = []
    warnings: list[str] = []
    low = html.lower()

    for bad, why in [
        ("<div", "must use <section>, not <div>"),
        ("<h1", "WeChat uses its own title — do not emit <h1>"),
        ("<style", "no <style> tag — all styles must be inline"),
        ("class=", "no class= attribute — all styles must be inline"),
    ]:
        if bad in low:
            errors.append(f"forbidden `{bad}` found ({why})")

    if "<!-- meta" in low or "<!--\nmeta" in low or "meta_title" in low:
        errors.append("leftover <!-- META --> head found — formatter must strip it")

    # Un-converted markdown.
    for i, ln in enumerate(html.splitlines(), 1):
        for rx in MD_LINE_RES:
            if rx.match(ln):
                errors.append(f"line {i}: un-converted markdown heading/list/quote: {ln.strip()[:60]!r}")
                break
    for rx in MD_ANYWHERE_RES:
        if rx.search(html):
            errors.append(f"un-converted markdown marker {rx.pattern!r} found in HTML")

    # Balanced <section> tags.
    n_open = len(SECTION_OPEN_RE.findall(html))
    n_close = len(SECTION_CLOSE_RE.findall(html))
    if n_open != n_close:
        errors.append(f"unbalanced <section>: {n_open} open vs {n_close} close")

    # Every <img> must carry a non-empty src.
    for tag in IMG_RE.findall(html):
        m = SRC_RE.search(tag)
        if not m or not m.group(1).strip():
            errors.append(f"<img> without non-empty src: {tag[:80]!r}")

    if len(html.encode("utf-8")) < 2000:
        warnings.append("HTML is <2000 bytes — possible truncation or near-empty output")

    return errors, warnings


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("file", type=Path)
    parser.add_argument("--json", action="store_true", help="Emit machine-readable result")
    args = parser.parse_args()
    try:
        html = args.file.expanduser().read_text(encoding="utf-8")
    except OSError as exc:
        raise SystemExit(f"Cannot read HTML: {exc}") from exc
    errors, warnings = validate(html)
    result = {"ok": not errors, "errors": errors, "warnings": warnings}
    if args.json:
        print(json.dumps(result, ensure_ascii=False, indent=2))
    else:
        for w in warnings:
            print(f"WARNING:{w}")
        for e in errors:
            print(f"ERROR:{e}")
        print("HTML_OK" if not errors else f"HTML_INVALID:{len(errors)}")
    return 0 if not errors else 1


if __name__ == "__main__":
    sys.exit(main())
