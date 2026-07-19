---
name: office-docx
description: 生成 Word 文档 DOCX（报告、合同、纪要、公文），python-docx 写脚本落盘 output/
version: 1
privilege: system
tools:
  - file_write
  - file_list
  - file_read
  - shell_exec
shell_allow:
  - "python3 output/scripts/*"
  - "python output/scripts/*"
  - "pip3 install python-docx"
  - "pip install python-docx"
---

# Office DOCX — 生成 Word 文档

当用户需要 **Word / DOCX / 报告 / 合同草稿 / 会议纪要** 等可编辑文档时使用本 flow。  
这是**系统共享能力**，任何分身都可命中。

## 硬规则

1. 产物路径必须在 **`output/`** 下（如 `output/report.docx`），禁止写 `/tmp`。
2. 生成脚本写到 `output/scripts/gen_docx_<slug>.py`，再 `shell_exec` 执行。
3. 执行成功后告诉用户 **本地路径**；若工具返回 `view_url` 必须原样贴给用户。
4. 中文默认字体优先：`PingFang SC` / `Songti SC` / `Noto Sans CJK SC` / `WenQuanYi Micro Hei`（按环境可用者）。
5. 缺库时报错里写：`pip3 install python-docx`，不要 silently fail。

## Process

### 1. 澄清需求（缺什么问什么，已知则跳过）

- 文档用途与读者
- 结构：标题层级、章节、是否表格/列表
- 是否有用户提供的正文/数据（可先 `file_read`）

### 2. 写生成脚本

`file_write` 保存 Python 脚本，使用 **python-docx**：

```python
# output/scripts/gen_docx_example.py
from docx import Document
from docx.oxml.ns import qn
from docx.shared import Pt, Cm
import os

OUT = os.path.join(os.path.dirname(__file__), "..", "report.docx")
OUT = os.path.normpath(OUT)

def set_run_font(run, name="PingFang SC", size=11):
    run.font.name = name
    run._element.rPr.rFonts.set(qn("w:eastAsia"), name)
    run.font.size = Pt(size)

doc = Document()
# 页边距
for section in doc.sections:
    section.top_margin = Cm(2.54)
    section.bottom_margin = Cm(2.54)
    section.left_margin = Cm(3.17)
    section.right_margin = Cm(3.17)

h = doc.add_heading("标题", level=1)
for r in h.runs:
    set_run_font(r, size=16)

p = doc.add_paragraph()
r = p.add_run("正文段落……")
set_run_font(r)

# 表格示例
# table = doc.add_table(rows=2, cols=2)
# table.style = "Table Grid"

os.makedirs(os.path.dirname(OUT), exist_ok=True)
doc.save(OUT)
print("OK", OUT)
```

按用户内容改标题、段落、表格；**不要**把用户隐私写进知识库。

### 3. 执行

```text
shell_exec(command="python3 output/scripts/gen_docx_<slug>.py", timeout_seconds=60)
```

工作目录为当前分身 workspace 根。

### 4. 交付

- 确认 `output/*.docx` 存在（`file_list("output/")`）
- 回复结构：

```markdown
## Word 已生成
- 文件：`output/xxx.docx`
- 链接：{view_url 若有}
- 说明：章节/表格结构一句话
```

## Important Principles

- 复杂排版可先生成 Markdown 再 `file_convert` 转 docx（简单场景）；规范公文/多级标题优先 python-docx。
- 不编造法律/财务结论；用户未给的数据用占位符并标明。
- 一次任务一个主文件，文件名用英文/拼音 slug + 日期可选。
