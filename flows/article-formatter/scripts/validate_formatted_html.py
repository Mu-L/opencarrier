#!/usr/bin/env python3
"""Validate a writing-pipeline 正文.html — the WeChat-compliant inline-styled HTML
emitted by article-formatter (shape only; visual aesthetics is human review's
job). HTML_OK / HTML_INVALID:N + return 0/1.

HTML validity is the most deterministically checkable step in the chain, so
this gate is the strictest. It enforces the formatter's own post-processing
rules: only ``<section>`` (no ``<div>``), no ``<h1>``, no ``<style>``/``class=`` (all
inline), no leftover ``<!-- META -->`` head, no un-converted markdown, balanced
``<section>`` tags, and every ``<img>`` carries a non-empty src. Pure structure.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

# Shared utility (validate_util.py at ~/.opencarrier/flows/).
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import validate_util  # noqa: E402


def validate(text: str) -> tuple[list[str], list[str]]:
    errors: list[str] = []
    warnings: list[str] = []

    if "<div" in text:
        errors.append("contains <div> — must use <section> only (formatter contract)")
    if "<h1" in text:
        errors.append("contains <h1> — no h1 in article body")
    if "<style" in text:
        errors.append("contains <style> — all styles must be inline")
    if 'class=' in text:
        errors.append("contains class= — all styles must be inline")

    if "<!-- META" in text:
        errors.append("contains leftover <!-- META --> head — formatter must strip it")

    md_patterns = [
        (r"^#{1,6}\s", "markdown heading"),
        (r"\*\*", "markdown bold (**)"),
        (r"`[^`]+`", "inline code"),
        (r"\[([^\]]+)\]\(([^)]+)\)", "markdown link"),
        (r"^- ", "markdown list item"),
        (r"^> ", "blockquote"),
    ]
    for pattern, name in md_patterns:
        if re.search(pattern, text, re.MULTILINE):
            errors.append(f"contains un-converted markdown ({name})")

    section_opens = len(re.findall(r"<section", text))
    section_closes = len(re.findall(r"</section>", text))
    if section_opens != section_closes:
        errors.append(
            f"<section> open/close mismatch: {section_opens} opens vs {section_closes} closes"
        )

    img_srcs = re.findall(r'<img[^>]*\bsrc\s*=\s*"([^"]*)"', text)
    for src in img_srcs:
        if not src.strip():
            errors.append(f"<img> with empty src: {src!r}")

    if len(text.encode("utf-8")) < 2000:
        warnings.append("HTML is <2000 bytes — possible truncation or empty output")

    return errors, warnings


if __name__ == "__main__":
    sys.exit(validate_util.run_main(validate, "HTML", __doc__))
