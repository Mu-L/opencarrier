---
name: short-video
description: 根据主题生成短视频(研究→脚本→图片→配音→合成mp4)，适合小红书/抖音/视频号
version: 1
tools:
  - file_read
  - file_write
  - shell_exec
  - image_generate
  - web_search
---

# Short Video Creator — 短视频生成

根据用户提供的主题，生成完整的短视频（mp4），适合发布到小红书、抖音、视频号等平台。

## Process

### Step 1: 研究主题

用 `web_search` 搜索主题相关的：
- 热门讨论和争议点
- 具体数据和统计
- 已有的热门内容角度

输出：3-5 个关键事实 + 一个独特的切入角度。

### Step 2: 写脚本

写一段 150-250 字的旁白脚本。要求：
- **钩子**（前 2 句抓住注意力）
- **主体**（核心信息，配合画面）
- **收尾**（号召行动或引发思考）
- 每 30-50 字标记一个 `[SCENE]` 作为换图点

保存到 `output/short-video/script.txt`。

### Step 3: 生成画面

根据脚本的每个 `[SCENE]` 节，用 `image_generate` 生成对应的图片。要求：
- 同一视频所有图片保持风格一致（在 prompt 中指定统一风格词，如"扁平插画风格、干净简约"）
- 生成 3-5 张图，分别保存到 `output/short-video/img_01.png` ~ `img_05.png`

### Step 4: 生成配音

用下面的 Python 脚本生成语音。先 `file_write` 脚本内容到 `output/short-video/gen_tts.py`，然后 `shell_exec` 执行。

```python
# gen_tts.py — 将脚本转为语音 mp3
import asyncio, edge_tts, sys, os

TEXT = """{{SCRIPT_TEXT}}"""  # 替换为实际脚本全文
VOICE = "zh-CN-XiaoxiaoNeural"  # 中文女声，可换成 zh-CN-YunxiNeural(男声)
OUTPUT = os.path.join(os.path.dirname(__file__), "narration.mp3")

async def main():
    communicate = edge_tts.Communicate(TEXT, VOICE)
    await communicate.save(OUTPUT)
    print(f"Narration saved to {OUTPUT}")

asyncio.run(main())
```

执行：`shell_exec(command="python3 output/short-video/gen_tts.py", timeout_seconds=60)`

### Step 5: 合成视频

用下面的 Python 脚本合成。先 `file_write` 到 `output/short-video/compose.py`，然后执行。

```python
# compose.py — 图片+音频合成为 mp4
from PIL import Image
from moviepy import AudioFileClip, ImageClip, concatenate_videoclips
from moviepy.video.fx import FadeIn, FadeOut
import os, glob

WORK_DIR = os.path.dirname(__file__)
AUDIO_FILE = os.path.join(WORK_DIR, "narration.mp3")
OUTPUT = os.path.join(WORK_DIR, "final.mp4")

# 收集所有 img_*.png 图片，按文件名排序
images = sorted(glob.glob(os.path.join(WORK_DIR, "img_*.png")))
if not images:
    print("ERROR: No images found! Expected img_01.png ~ img_05.png")
    exit(1)

# 加载音频获取总时长
audio = AudioFileClip(AUDIO_FILE)
duration_per_image = audio.duration / len(images)

print(f"Total duration: {audio.duration:.1f}s, {len(images)} images, {duration_per_image:.1f}s each")

# 每张图生成视频片段（缩放+淡入淡出）
clips = []
target_w, target_h = 1080, 1920  # 竖屏 9:16

for i, img_path in enumerate(images):
    # 用 PIL 处理图片：缩放填充到目标尺寸
    img = Image.open(img_path).convert("RGB")
    img_ratio = img.width / img.height
    target_ratio = target_w / target_h

    if img_ratio > target_ratio:
        new_h = target_h
        new_w = int(img.width * (target_h / img.height))
        img = img.resize((new_w, new_h), Image.LANCZOS)
        left = (new_w - target_w) // 2
        img = img.crop((left, 0, left + target_w, target_h))
    else:
        new_w = target_w
        new_h = int(img.height * (target_w / img.width))
        img = img.resize((new_w, new_h), Image.LANCZOS)
        top = (new_h - target_h) // 2
        img = img.crop((0, top, target_w, top + target_h))

    temp_path = os.path.join(WORK_DIR, f"_temp_{i}.png")
    img.save(temp_path)

    clip = ImageClip(temp_path, duration=duration_per_image)
    # 淡入淡出
    if i == 0:
        clip = clip.with_effects([FadeOut(0.3)])
    elif i == len(images) - 1:
        clip = clip.with_effects([FadeIn(0.3)])
    else:
        clip = clip.with_effects([FadeIn(0.3), FadeOut(0.3)])

    clips.append(clip)
    os.remove(temp_path)

# 拼接所有片段
final = concatenate_videoclips(clips, method="compose")
final = final.with_audio(audio)

# 渲染输出
final.write_videofile(
    OUTPUT, fps=24, codec="libx264", audio_codec="aac",
    preset="medium", bitrate="5000k", threads=4
)

# 清理
for clip in clips:
    clip.close()
audio.close()
final.close()

print(f"Video saved to {OUTPUT}")
```

执行：`shell_exec(command="python3 output/short-video/compose.py", timeout_seconds=120)`

### Step 6: 输出

视频最终路径：`output/short-video/final.mp4`

回复用户时告知：
- 视频时长
- 文件路径
- 可以继续调整的地方（换图、改脚本、调整节奏）

## 注意事项

- **edge-tts 已安装**在服务器上，直接使用
- 如果用户想换声音，可选 VOICE 包括：`zh-CN-XiaoxiaoNeural`（女）、`zh-CN-YunxiNeural`（男）、`zh-CN-XiaoyiNeural`（活泼女）
- **图片生成 prompt 要英文**（image_generate 的 LLM 用英文效果更好）
- 默认竖屏 9:16（1080x1920），用户要横屏（16:9）改 compose.py 的 target_w/h
