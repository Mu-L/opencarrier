---
name: office-xlsx
description: 生成 Excel 表格 XLSX（报表、清单、统计、带公式/简单图表），openpyxl 写脚本落盘 output/
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
  - "pip3 install openpyxl"
  - "pip install openpyxl"
---

# Office XLSX — 生成 Excel 表格

当用户需要 **Excel / 表格 / 报表 / 清单 / 统计表 / XLSX** 时使用。  
系统共享能力，任意分身可命中。

## 硬规则

1. 产物必须在 **`output/`**（如 `output/report.xlsx`），禁止 `/tmp`。
2. 脚本：`output/scripts/gen_xlsx_<slug>.py` → `shell_exec python3 …`
3. 成功后贴路径；有 `view_url` 必须贴给用户。
4. 默认库：**openpyxl**（读写真齐）。大数据/图表优先仍可用 openpyxl。
5. 缺库：`pip3 install openpyxl`

## Process

### 1. 澄清

- 列结构、表头、样例数据
- 是否需要公式、多 sheet、简单图表
- 数据来源：用户粘贴 / 文件 / 需占位示例

### 2. 写脚本

```python
# output/scripts/gen_xlsx_example.py
from openpyxl import Workbook
from openpyxl.styles import Font, Alignment, Border, Side, PatternFill
import os

OUT = os.path.normpath(os.path.join(os.path.dirname(__file__), "..", "report.xlsx"))

wb = Workbook()
ws = wb.active
ws.title = "数据"

headers = ["项目", "数量", "金额"]
ws.append(headers)
header_font = Font(bold=True)
for cell in ws[1]:
    cell.font = header_font
    cell.alignment = Alignment(horizontal="center")

# 数据行
ws.append(["示例A", 10, 100.0])
ws.append(["示例B", 5, 50.0])
# 公式
ws["C4"] = "=SUM(C2:C3)"
ws["A4"] = "合计"

# 列宽
for col in ("A", "B", "C"):
    ws.column_dimensions[col].width = 14

os.makedirs(os.path.dirname(OUT), exist_ok=True)
wb.save(OUT)
print("OK", OUT)
```

把用户真实数据写入；金额用数字类型，不要全当字符串。

### 3. 执行与交付

```text
shell_exec(command="python3 output/scripts/gen_xlsx_<slug>.py", timeout_seconds=60)
```

```markdown
## Excel 已生成
- 文件：`output/xxx.xlsx`
- 链接：{view_url}
- Sheet / 列说明
```

## Important Principles

- 不编造业务 KPI；无数据时生成带「示例数据」水印式说明的模板表。
- 多表用多个 sheet，命名清晰。
- 图表仅在用户明确要求时添加，保持简单可读。
