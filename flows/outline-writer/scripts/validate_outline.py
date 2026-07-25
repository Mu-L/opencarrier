#!/usr/bin/env python3
"""Validate a writing-pipeline 大纲.md (shape only; outline quality / writing-style
adherence is the system prompt + human review's job). Mirrors the
validate_*.py convention: OUTLINE_OK / OUTLINE_INVALID:N markers + return 0/1.

Checks the structural contract that article-writer inherits: a well-formed
META head (title/author/digest/type/pipeline), the three required sections
(标题备选 >=3 / 核心论点 / 文章结构), no pipeline-id leaking into the body, and no
leftover placeholders. Pure structure — never judges whether the outline is *good*.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

# Shared utility (validate_util.py at ~/.opencarrier/flows/).
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import validate_util  # noqa: E402

VALID_TYPES = {"行业分析", "热点评论", "产品文章", "深度教程"}
PIPELINE_RE = re.compile(r"pipeline-\d{8}-")
PLACEHOLDERS = ("待补充", "TODO", "TBD", "占位", "XXXX", "xxx")


def section_lines(body: str, name: str) -> list[str] | None:
    """Lines under a ``## <name>`` heading until the next ``## `` heading."""
    out: list[str] = []
    in_sec = False
    for ln in body.splitlines():
        stripped = ln.strip()
        if stripped.startswith("## "):
            if in_sec:
                break
            if stripped[3:].strip() == name:
                in_sec = True
            continue
        if in_sec:
            out.append(ln)
    return out if in_sec else None


def validate(text: str) -> tuple[list[str], list[str]]:
    errors: list[str] = []
    warnings: list[str] = []
    meta, body = validate_util.parse_meta_head(text)

    if not meta:
        errors.append("missing <!-- META --> head at file start (outline-writer requires it)")
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

    titles = section_lines(body, "标题备选")
    if titles is None:
        errors.append("missing `## 标题备选` section")
    else:
        numbered = [t for t in titles if re.match(r"\s*\d+[.、)]", t)]
        if len(numbered) < 3:
            errors.append(f"`## 标题备选` needs >=3 numbered title candidates (got {len(numbered)})")
        for t in numbered:
            if validate_util.cjk_len(t) < 4:
                warnings.append(f"short title candidate (len<4): {t.strip()!r}")

    for sec in ("核心论点", "文章结构"):
        lines = section_lines(body, sec)
        if lines is None:
            errors.append(f"missing `## {sec}` section")
        elif not any(l.strip() for l in lines):
            errors.append(f"`## {sec}` section is empty")

    if "流水线ID:" in body:
        errors.append("`流水线ID:` must not appear in the body — keep it in the META head only")

    found = [p for p in PLACEHOLDERS if p in body]
    if found:
        errors.append(f"leftover placeholders in body: {found}")

    return errors, warnings


if __name__ == "__main__":
    sys.exit(validate_util.run_main(validate, "OUTLINE", __doc__))
