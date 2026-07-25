#!/usr/bin/env python3
"""Validate a writing-pipeline 正文.md (shape only; prose quality / writing-style
is the system prompt + human review's job). ARTICLE_OK / ARTICLE_INVALID:N + 0/1.

Checks the structural contract downstream (article-formatter) relies on: a
well-formed META head, a single H1 title under it, >=1 H2 section, a per-type
word-count floor, no pipeline-id leaking into the body, and no leftover
placeholders. Pure structure — never judges whether the article is *good*.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

# Shared utility (validate_util.py at ~/.opencarrier/flows/).
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import validate_util  # noqa: E402

VALID_TYPES = {"行业分析", "热点评论", "产品文章", "深度教程"}
WORD_FLOOR = {"行业分析": 1500, "热点评论": 800, "产品文章": 1000, "深度教程": 1500}
PIPELINE_RE = re.compile(r"pipeline-\d{8}-")
PLACEHOLDERS = ("待补充", "TODO", "TBD", "XXXX", "占位符")


def first_body_line(body: str) -> str:
    for ln in body.splitlines():
        if ln.strip():
            return ln.strip()
    return ""


def validate(text: str) -> tuple[list[str], list[str]]:
    errors: list[str] = []
    warnings: list[str] = []
    meta, body = validate_util.parse_meta_head(text)

    if not meta:
        errors.append("missing <!-- META --> head at file start (article-writer requires it)")
        return errors, warnings

    for key in ("title", "author", "digest", "type", "pipeline"):
        if not meta.get(key):
            errors.append(f"META_{key.upper()} must be present and non-empty")

    digest = meta.get("digest", "")
    if digest:
        n = validate_util.cjk_len(digest)
        if n < 30 or n > 60:
            errors.append(f"META_DIGEST must be 30-60 chars (got {n})")

    mtype = meta.get("type", "")
    if mtype and mtype not in VALID_TYPES:
        errors.append(f"META_TYPE must be one of {sorted(VALID_TYPES)} (got {mtype!r})")

    pipeline = meta.get("pipeline", "")
    if pipeline and not PIPELINE_RE.search(pipeline):
        errors.append(f"META_PIPELINE must match pipeline-YYYYMMDD-<topic> (got {pipeline!r})")

    first = first_body_line(body)
    if not first.startswith("# "):
        errors.append("body must start with an H1 title (`# ...`) after the META head")
    elif meta.get("title") and first[2:].strip() != meta["title"]:
        warnings.append(f"H1 title {first[2:].strip()!r} != META_TITLE {meta['title']!r} (should inherit)")

    h2 = [ln for ln in body.splitlines() if ln.strip().startswith("## ")]
    if not h2:
        errors.append("article body has no `## ` H2 section (a wall of text is not an article)")

    if mtype in WORD_FLOOR:
        n = validate_util.cjk_len(body)
        floor = WORD_FLOOR[mtype]
        if n < floor:
            errors.append(f"word count {n} below {mtype} floor {floor} (non-space chars)")

    if "流水线ID:" in body:
        errors.append("`流水线ID:` must not appear in the body — keep it in the META head only")

    found = [p for p in PLACEHOLDERS if p in body]
    if found:
        errors.append(f"leftover placeholders in body: {found}")

    return errors, warnings


if __name__ == "__main__":
    sys.exit(validate_util.run_main(validate, "ARTICLE", __doc__))
