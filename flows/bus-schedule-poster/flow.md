---
name: bus-schedule-poster
description: 生成86巴士班次时刻表宣传海报图（竖版），严格按官方蓝白模板排版，文字清晰可印刷
version: 1
privilege: system
max_iterations: 12
tools:
  - file_write
  - file_read
  - file_list
  - knowledge_read
  - shell_exec
shell_allow:
  - "python3 output/scripts/*"
  - "python output/scripts/*"
  - "cp *bus-schedule-poster/gen_poster.py output/scripts/*"
  - "cp *gen_poster.py output/scripts/*"
  - "pip3 install pillow*"
  - "pip install pillow*"
---

# 86巴士 · 班次时刻表海报

当用户要做 **班次海报 / 时刻表海报 / 线路宣传图 / 新线海报 / 通勤专线海报图** 时使用本 flow。  
**禁止**用 `image_generate` 画带时刻文字的海报（AI 会糊字、版式漂、和参考图对不上）。  
**必须**用 Pillow 模板脚本精确排版（对齐官方参考：蓝白底 + 上蓝波 + 去程绿点 / 回程红点 + 底部二维码）。

## 硬规则

1. **主交付 = PNG 海报**，路径 `output/posters/<slug>.png`，回复必须贴 `view_url`。
2. **禁止** `image_generate` 作为主路径（除非用户只要「氛围促销图、不要精确时刻」才可辅用，且需先说明文字可能不准）。
3. 数据优先：`knowledge_read`（如 `baidu-commute.md`）→ 用户消息 → 用户上传参考图/文字。
4. 版式固定，只改**数据**（线路名、时刻、站点、票价、二维码），不要改品牌色和整体布局。
5. 脚本写到 `output/scripts/gen_86_schedule_poster.py`（可直接复制本 flow 配套 `gen_poster.py` 逻辑），再 `shell_exec` 运行。

## 官方视觉规范（锁死）

| 元素 | 规范 |
|------|------|
| 画布 | 1080×1680（竖版，适合微信） |
| 背景 | 浅冷白蓝 `#F5F8FF`，顶部柔和蓝色光斑 |
| 品牌 | 左上角「86巴士」深蓝 |
| 主标题 | 大号蓝色「新线试运行时刻表」或用户指定标题 |
| 副标题 | 线路名，如 `市桥 ⇄ 百度广州` |
| 角标 | 可选橙色 `NEW` |
| 左栏 | **去程**，绿点 = 上车，红点 = 下车 |
| 右栏 | **回程**，同上 |
| 底栏 | 深蓝圆角条：票价 / 满员发车 / 小程序购票 |
| 右下 | 二维码区（有图用 `qr_path`，无图用占位格） |
| 水印 | 浅色重复「86巴士」 |

参考图特征（用户常发的官方样张）：**不是**红绿撞色促销风，**是**干净蓝白信息图。

## Process

### 1. 收数

一次问清缺项（有 knowledge 则先读再补）：

- 线路名 / 标题
- 去程：发车时间 + 站点列表（可带每站时刻）
- 回程：同上
- 票价、满员人数、购票方式
- 是否有官方小程序码图片路径（`input/` 下）

### 2. 写配置 JSON

`file_write` → `output/scripts/poster_config.json`：

```json
{
  "brand": "86巴士",
  "title": "新线试运行时刻表",
  "route": "百度广州通勤专线 · 市桥 往返 百度广州",
  "badge": "NEW",
  "footer": "票价 ¥10/人  ·  满40/38人发车  ·  86巴士小程序购票",
  "qr_path": "",
  "qr_label": "长按识别二维码\n立即购票",
  "outbound": {
    "label": "去程",
    "time": "07:35",
    "note": "发车",
    "stops": [
      {"name": "市桥地铁口", "type": "board", "time": "07:35"},
      {"name": "沙头新村", "type": "board", "time": "07:40"},
      {"name": "百度广州公司", "type": "alight", "time": ""}
    ]
  },
  "inbound": {
    "label": "回程",
    "time": "18:30",
    "note": "发车",
    "stops": [
      {"name": "百度广州公司", "type": "board", "time": "18:30"},
      {"name": "沙头新村", "type": "alight", "time": ""},
      {"name": "市桥地铁口", "type": "alight", "time": ""}
    ]
  }
}
```

`type`：`board`=上车（绿），`alight`=下车（红）。

### 3. 准备生成脚本

**优先**从系统 flow 目录复制现成模板（不要自己从零写、不要用 AI 生图）：

```bash
cp ~/.opencarrier/flows/bus-schedule-poster/gen_poster.py output/scripts/gen_86_schedule_poster.py
```

若复制失败，再 `file_write` 完整 Pillow 脚本（蓝白模板 + 双栏时刻线 + 底栏 + 二维码区；字体 Noto Sans CJK / 文泉驿）。

### 4. 执行

```
shell_exec(command="python3 output/scripts/gen_86_schedule_poster.py --config output/scripts/poster_config.json --out output/posters/<slug>.png", timeout_seconds=60)
```

缺库时先：`pip3 install pillow`（仅 pillow；二维码可选）。

### 5. 交付

- 贴 **view_url**
- 一句话说明线路 + 去程/回程要点
- 若用户还要可编辑表：可另走 `office-xlsx`，不要混进本海报流程

## 反例（禁止）

- ❌ 把海报任务分类成 office-pdf / schedule-chart（那是文档和排班覆盖图）
- ❌ `image_generate("blue poster with bus schedule...")` 当主交付
- ❌ 红绿对撞促销大字风（除非用户明确说「要促销大字、不要官方时刻表样式」）
- ❌ 中文用默认拉丁字体导致方框

## Important Principles

- **文字准确 > 画面炫**：班次海报是运营物料，时刻错了等于事故
- **模板优先**：改数据不改版式
- **有参考图时**：用 vision/描述只提取「数据与版式意图」，渲染仍走模板
