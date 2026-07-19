---
name: office-pptx
description: 生成 PowerPoint 演示文稿 PPTX（汇报、路演、培训幻灯片），python-pptx 写脚本落盘 output/
version: 1
tools:
  - file_write
  - file_list
  - file_read
  - shell_exec
---

# Office PPTX — 生成演示文稿

当用户需要 **PPT / 幻灯片 / 演示 / 路演 / 汇报材料 / PPTX** 时使用。  
系统共享能力。

## 硬规则

1. 产物 **`output/*.pptx`**，禁止 `/tmp`。
2. 脚本：`output/scripts/gen_pptx_<slug>.py`。
3. 交付贴路径 + `view_url`（若有）。
4. 库：**python-pptx**；缺库：`pip3 install python-pptx`
5. 默认 16:9：宽 13.333" × 高 7.5"（或 Inches(13.333), Inches(7.5)）。
6. 中文标题/正文设置东亚字体（PingFang SC / Noto Sans CJK SC 等）。

## Process

### 1. 澄清

- 受众与场合（内部汇报 / 客户 / 培训）
- 页数与大纲（每页一个要点）
- 是否有品牌色（未指定则用简洁深色标题 + 黑正文）

### 2. 结构建议

- 第 1 页：标题 + 副标题
- 中间：一页一主题（标题 + 3–6 条要点）
- 可选：数据页（表格）、结尾页（下一步 / Q&A）

### 3. 写脚本

```python
# output/scripts/gen_pptx_example.py
from pptx import Presentation
from pptx.util import Inches, Pt
from pptx.dml.color import RgbColor
from pptx.enum.text import PP_ALIGN
import os

OUT = os.path.normpath(os.path.join(os.path.dirname(__file__), "..", "deck.pptx"))

prs = Presentation()
prs.slide_width = Inches(13.333)
prs.slide_height = Inches(7.5)

# 标题页
slide = prs.slides.add_slide(prs.slide_layouts[0])
slide.shapes.title.text = "主标题"
slide.placeholders[1].text = "副标题 / 日期 / 汇报人"

# 内容页
slide2 = prs.slides.add_slide(prs.slide_layouts[1])
slide2.shapes.title.text = "章节标题"
tf = slide2.placeholders[1].text_frame
tf.clear()
bullets = ["要点一", "要点二", "要点三"]
tf.paragraphs[0].text = bullets[0]
tf.paragraphs[0].level = 0
for line in bullets[1:]:
    p = tf.add_paragraph()
    p.text = line
    p.level = 0

os.makedirs(os.path.dirname(OUT), exist_ok=True)
prs.save(OUT)
print("OK", OUT)
```

按大纲生成多页；避免一页堆砌大段文字。

### 4. 执行与交付

```text
shell_exec(command="python3 output/scripts/gen_pptx_<slug>.py", timeout_seconds=60)
```

```markdown
## PPT 已生成
- 文件：`output/xxx.pptx`
- 链接：{view_url}
- 页数与结构摘要
```

## Important Principles

- 一页一信息焦点；标题可扫读。
- 用户未提供数据时用占位并标明「示例」。
- 不自动配图除非用户要求（可用 `image_generate` 另存 output 再插入）。
