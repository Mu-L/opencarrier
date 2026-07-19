---
name: office-pdf
description: 生成 PDF 文档（报告、可打印材料、合同页），reportlab 写脚本落盘；不做班次海报图
version: 1
privilege: system
tools:
  - file_write
  - file_list
  - file_read
  - shell_exec
  - file_convert
shell_allow:
  - "python3 output/scripts/*"
  - "python output/scripts/*"
  - "pip3 install reportlab"
  - "pip install reportlab"
---

# Office PDF — 生成 PDF

当用户需要 **PDF / 可打印文档 / 正式对外 PDF** 时使用。  
系统共享能力。

## 硬规则

1. 产物 **`output/*.pdf`**，禁止 `/tmp`。
2. 脚本：`output/scripts/gen_pdf_<slug>.py`（reportlab）。
3. 简单 Markdown→PDF 可走 `file_convert`（Pandoc）；复杂排版用 reportlab。
4. 交付路径 + `view_url`。
5. 缺库：`pip3 install reportlab`；中文需系统字体。

## Process

### 1. 澄清

- 是否必须 PDF（有时 DOCX 更易改）
- 页数、纸张（A4 默认）、是否多页报告

### 2. 路径选择

| 场景 | 做法 |
|------|------|
| 已有 Markdown/HTML，版式简单 | `file_write` md → `file_convert` 出 pdf |
| 表格/精确定位/多段样式 | reportlab 脚本 |

### 3. reportlab 示例

```python
# output/scripts/gen_pdf_example.py
from reportlab.lib.pagesizes import A4
from reportlab.pdfgen import canvas
from reportlab.pdfbase import pdfmetrics
from reportlab.pdfbase.ttfonts import TTFont
from reportlab.lib.units import mm
import os

OUT = os.path.normpath(os.path.join(os.path.dirname(__file__), "..", "report.pdf"))

# 注册中文字体（按服务器实际路径调整，失败则用 Helvetica 仅英文）
FONT = "Helvetica"
for path, name in [
    ("/System/Library/Fonts/PingFang.ttc", "PingFang"),
    ("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc", "NotoSansCJK"),
    ("/usr/share/fonts/truetype/wqy/wqy-microhei.ttc", "WQY"),
]:
    if os.path.exists(path):
        try:
            pdfmetrics.registerFont(TTFont(name, path, subfontIndex=0))
            FONT = name
            break
        except Exception:
            pass

c = canvas.Canvas(OUT, pagesize=A4)
width, height = A4
c.setFont(FONT, 16)
c.drawString(25 * mm, height - 30 * mm, "标题")
c.setFont(FONT, 11)
c.drawString(25 * mm, height - 45 * mm, "正文内容……")
c.showPage()
c.save()
print("OK", OUT, "font=", FONT)
```

### 4. 执行与交付

```text
shell_exec(command="python3 output/scripts/gen_pdf_<slug>.py", timeout_seconds=60)
```

```markdown
## PDF 已生成
- 文件：`output/xxx.pdf`
- 链接：{view_url}
```

## Important Principles

- 中文乱码优先检查字体注册，不要反复重试同一脚本。
- 用户要「可再编辑」时建议改走 `office-docx`。
- 安全：不执行用户粘贴的不明 shell，只跑自己 file_write 的脚本。
