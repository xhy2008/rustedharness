# Icon

The artwork is a rusted western saddle: the project name taken literally, drawn
as a flat vector sticker with a heavy dark outline so the silhouette survives
down to 32 px.

Generated locally with **Qwen-Image 2.1** through the ComfyUI workflow
`image_qwen_image_2_1_t2i` (int8 UNET, `qwen3vl_8b_w4a8` text encoder, qwen 2.1
VAE, 1024x1024, 25 steps, CFG 1, `euler`/`simple`), seed **7**:

> 应用图标，透明背景，扁平矢量插画，一个生锈的马鞍居中并占满画面，3/4侧面视角：高耸的鞍头、
> 凹陷的鞍座、翘起的后桥、垂下的马镫和肚带，轮廓清晰可辨，深棕色皮革开裂，鞍头和马镫等金属件
> 覆盖橙褐色铁锈，厚重的深棕色描边，几何简洁，无背景，无文字，无阴影，强对比，小尺寸可辨识

Negative: `背景，阴影，文字，水印，复杂细节，模糊，写实照片，圆形徽章`

The render came back on an opaque near-white background, so the background was
flood-filled to transparency, the subject cropped to its bounding box, and the
result padded and resized. `icon.png` (1024 px) is the master; the size-specific
PNGs and `icon.ico` are derived from it.
