# docp 架构设计

> kitty 图形协议终端（macOS + Rex）上的 PDF / 图片 / Markdown 预览器。
> Rust 实现，大量代码继承自 tkmp（MIT，同作者生态）。
> 因链接 mupdf（AGPL-3.0），本项目按 AGPL-3.0 开源。

## 0. 目标与非目标

**目标**
- 在 Rex 分屏里跟随文件变化实时预览：`rex split -- docp plan.md`
- 三种格式一个引擎：统一滚动 / 缩放 / 搜索 / 链接 / 复制体验
- 零外部依赖：无 Node、无浏览器、无 poppler CLI、无系统字体要求（内嵌字体 + 系统字体兜底）
- 性能预算：冷启动 < 100ms；翻页 < 50ms；纯滚动 < 8ms/帧；md 编辑到出帧 < 30ms

**非目标（现阶段）**
- tmux / Linux / Windows 支持（代码不主动排斥，但不投入）
- EPUB / DJVU / office 文档
- 编辑能力

## 1. 总览

```
┌────────────────────────────────────────────────────────────────┐
│ docp (CLI/TUI)                                                 │
│  参数解析 → 终端能力探测 → 打开文档 → 事件循环 + 画笔           │
├────────────────────────────────────────────────────────────────┤
│ termui          raw mode / alt-screen / 输入解析(crossterm)     │
│                 cell 像素探测(ioctl → CSI 16t → 默认)           │
│                 SGR/OSC8/OSC52、BSU/ESU 同步更新                │
├────────────────────────────────────────────────────────────────┤
│ kgp (kitty graphics)                                           │
│                 能力查询(a=q+DA1, e=T,U=1 特性查询)             │
│                 传输(t=d 分块 base64；t=f 文件为可选优化)       │
│                 Unicode placeholder 编码(U+10EEEE+变音符)       │
│                 垂直分块(>280 行切多 placement)                 │
│                 a=p,U=1 终端侧缩放 / a=d 删除 / id 分配         │
├────────────────────────────────────────────────────────────────┤
│ view (引擎核心，不 touch 终端，可 headless bench)               │
│   FlowView(md) │ PagedView(pdf/image)                           │
│   视口/滚动/缩放/锚定 · 行级 dirty diff · media 生命周期        │
├────────────────────────────────────────────────────────────────┤
│ docmodel       Row = Text(Line{cells,fill}) | Image{media_id}   │
│                 CJK 折行/禁则/中西文间距 · 主题                 │
├───────────────┬────────────────────┬───────────────────────────┤
│ render-md     │ render-img         │ render-pdf                │
│ pulldown-cmark│ image 解码+缩放    │ mupdf 视口分块光栅化       │
│ → 文本块(SGR) │ SVG→resvg          │ 文本层/大纲/链接/搜索      │
│ Mermaid(QuickJS+resvg) │ 动图帧   │ 高亮矩形合成进 tile        │
│ 大字标题光栅   │                    │ LRU+相邻页预取            │
│ 代码高亮(syntect, 文本路径) │ 公式(typst, M2+)                  │
└───────────────┴────────────────────┴───────────────────────────┘
```

关键原则：**像素只传一次，之后一切靠文本网格**。图片用 Unicode placeholder 挂在文本网格里，滚动/重绘/resize 由终端自己完成，我们只 diff 文本行。

## 2. Crate 划分与 tkmp 复用映射

```
kitty-doc-preview/
├─ Cargo.toml              # workspace
├─ crates/kgp/             # kitty graphics 协议库（可独立发布）
├─ crates/termui/          # 终端 UI 基础设施
├─ crates/docmodel/        # 统一文档模型 + 文本排版
├─ crates/render-md/       # markdown 混合渲染
├─ crates/render-pdf/      # PDF 后端（mupdf）
├─ crates/render-img/      # 图片后端
└─ crates/docp/            # 前端：CLI/TUI/bench
```

| tkmp 源文件 | 去向 | 说明 |
|---|---|---|
| `src/kitty.rs` (110) | kgp | 原样迁移 + 扩展（能力查询、分块、t=f） |
| `src/term.rs` (163) | termui | 尺寸/cell 探测、raw mode |
| `src/text.rs` (444) | docmodel | Line/Cell/Attrs，加 CJK 折行 |
| `src/theme.rs` (124) | docmodel | 终端配色查询 + 主题 |
| `src/fonts.rs` (168) | render-md(共享) | 内嵌字体 + fontdb 系统字体 |
| `src/raster.rs` (205) | render-md | 大字标题、resvg 画布（cell 整数倍） |
| `src/pele.rs`+`svgflat.rs` (341) | render-md | Mermaid（QuickJS 内嵌 Pele） |
| `src/doc.rs` (452) | render-md | 解析 → 块模型 |
| `src/app.rs` (883) | docp | 事件循环/画笔/diff，改造支持 PagedView |
| `src/bench.rs` (151) | docp | headless 计时 |

约 3.0k 行里 ~2.5k 直接复用。

## 3. 统一文档模型

一切格式最终产出同一种**显示列表**，view 和画笔对格式无感知：

```rust
enum Row {
    Text(Line),                            // styled cells + OSC8 链接 + 行填充色
    Image { media_id: u32, row: u16, cols: u16 },  // placeholder 行
}
struct Line { cells: Vec<Cell>, fill: Option<Rgb> }
struct Cell  { text: String, attrs: Attrs, url: Option<String> }

struct Media {
    id: u32,                 // 随机起点，原子递增（避免与其它程序冲突）
    key: Hash,               // 内容哈希：媒体去重与缓存键
    cols: u16, rows: u16,    // 占位尺寸（cell 单位）
    state: MediaState,       // Requested / Transmitting / OnScreen
    png: Option<Arc<Vec<u8>>>,
}
```

两种视图状态共享同一画笔：

- **FlowView**（markdown）：无限滚动的块流。`relayout(width)` 只重排受影响块（块级哈希 diff）。
- **PagedView**（PDF / 单图）：页序列 + 页分隔行 + 页码行。每页高度 = 视口宽下的 cell 行数，`scroll_y` 全文档连续，页码/行号双向换算。

## 4. 终端后端（kgp）

### 4.1 能力探测（启动时一次性）

```
1. ioctl TIOCGWINSZ → rows/cols/像素；ws_xpixel=0 时 fallback CSI 14t
2. CSI 16t → 单 cell 像素尺寸（失败用像素/cols 估算，再失败默认 8×16）
3. \x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\ + \x1b[c   → 图形协议是否支持
4. \x1b_Ga=q,i=32,e=T,U=1\x1b\                          → Unicode placeholder 支持
5. 探测 t=f：\x1b_Gi=33,a=q,t=f,f=100,S=..;<b64 缓存路径>\x1b\ → 文件传输可用性
```

Rex 的 `TERM=xterm-256color` 无信息量，**一律靠查询，不猜 TERM**（tkmp 已验证此路径）。

### 4.2 图片传输与放置

- 传输：`a=T,U=1,f=100,i=id,p=1,c=cols,r=rows,q=2,m=..` + base64 PNG 4096 分块（默认）；`t=f` 文件传输作为探测通过后的快路径（大 PDF tile 明显省 base64 开销）。
- 放置：placeholder 行 = `\e[38;2;r;g;bm` + `U+10EEEE` + 变音符(row,col) + 后续 cell 继承。图片 ID 走前景色（24bit）。
- **297 行上限**：变音符表长度限制单 placement 高度。> 280 行的图（长截图、放大后的 PDF 页）垂直切成多个 media，相邻摆放，对上层透明（`render-pdf`/`render-img` 负责 tile 化）。
- resize：先 `a=p,U=1,i=id,c,r` 让终端用已有像素立即重采样（零延迟），80ms 尺寸稳定后再按新像素密度重光栅重传（防抖，tkmp 已验证）。
- 删除：滚出视口 + LRU 淘汰的 media 发 `a=d,d=I,i=id`；帧内先删后传避免配额抖动（kitty 配额 320MB/缓冲）。
- 每帧输出包在 `ESC[?2026h … ESC[?2026l`（同步更新）里，Rex 支持（tkmp 在用）。

### 4.3 帧绘制流水

```
view.compute_visible(scroll, size)
  → 与上一帧行级 diff（只有变更行重写）
  → media 集合 diff：新增→传输作业；消失→删除
  → [BSU] 删除序列 + 传输序列 + 文本/placeholder 行 [ESU]
  → 单次 write 刷出
```

## 5. 格式后端

### 5.1 render-img（最简单，M0 先行）

PNG/JPEG/WebP/GIF/BMP/TIFF + SVG（resvg）。
- 解码 → 按视口宽度缩放（物理像素，cell 整数倍画布）→ PNG 编码（flate2/zlib）→ media
- GIF/WebP 动图：M3 用 `a=f` 帧协议做终端驱动播放（协议原生支持 gap 与循环，客户端不用留活）
- 单图 = 一页 PagedView；EXIF 方向修正；超大图 tile 化

### 5.2 render-md（混合渲染）

| 块类型 | 路径 | 可搜索/复制 |
|---|---|---|
| 正文/列表/引用/行内样式 | 文本 + SGR | ✅ |
| 代码块 | 文本 + syntect 高亮 | ✅ |
| GFM 表格 | 文本 + box-drawing（CJK 对齐） | ✅ |
| 链接 | OSC8 | ✅ |
| H1/H2 大字标题 | 像素（内嵌 Inter/系统 CJK 字体光栅） | ❌（可切 `--flat-headings`） |
| 图片 | 像素（内容哈希缓存，宽度自适应） | ❌ |
| Mermaid | 像素：Pele(QuickJS 内嵌) 布局 → resvg 光栅 | ❌ |
| 数学公式 | M2+：typst → SVG → resvg | ❌ |

要点：**默认路径是文本**，只有"等宽网格表达不了的东西"才进像素。md 编辑时块级 xxhash diff 保证只重光栅变化的块（tkmp 实测 19ms）。

### 5.3 render-pdf（mupdf）

```rust
trait PdfBackend {          // 抽象留给未来 pdfium 备胎，先只写 mupdf
    fn page_count(&self) -> usize;
    fn page_size(&self, p: usize) -> (f32, f32);                  // pt
    fn render(&self, p: usize, clip: PxRect, zoom: f32, hl: &[HlRect]) -> Rgba;
    fn text(&self, p: usize) -> Arc<PageText>;                    // span+rect 缓存
    fn outline(&self) -> Vec<OutlineNode>;
    fn links(&self, p: usize) -> Vec<Link>;
}
```

**视口分块光栅化**（性能核心，M2 已实现）：
- 页宽 = 视口 cell 宽 → 物理像素宽；缩放 = 像素宽/页宽 pt（fit-width 默认，`z/Z` 缩放倍率）
- 只光栅化**可见矩形 ± 半屏预取**，输出 ≤280 行的 tile；缓存键 `(page, zoom, tile_idx, px_w)`
- 实现：`render-pdf` 只暴露 `render_band(page, scale, x0, y0, w, h)`，用 draw device 的
  pixmap 边界裁剪，不整页光栅化；tile 高 64 行 + 1 行裁剪余量，LRU 32 项并设 48M 像素
  预算，跨页 ±1 预取。实测 2170×2048 px tile（含 PNG 编码）约 10–15 ms。`z/Z` 改变
  scale 后按 x 中心锚定，水平用 `h/l` 平移（不放大旧位图）。
- **放置（平滑滚动）**：tile 用 `a=t` 只传输不显示，再用 `a=p` + `x/y/w/h` 源裁剪放到
  `c×r` cell 盒。滚动量拆成整数行 + `frac` 像素；`frac` 只改 placement 的源裁剪，
  不重传像素、不重绘文本 → **亚 cell 平滑滚动**。每帧只发 2–3 条 placement；tile 多
  光栅化底部一行作为 `frac` 余量。placement id 固定，同 `(image, placement)` 原位覆盖；
  离开视口时 `a=d,d=i,p=1` 删除 placement，图像留在缓存里。
- mupdf `Document` 非 Sync：**单一渲染线程持有文档**，主线程投递 job（channel），结果带 media id 回主线程。text/outline 提取在同线程做首次缓存。实测单 tile 10–20ms，足够；不够再开多文档副本分片
- **依赖坑**：`mupdf` crate 默认开 `js` feature，会把 MuPDF 自带 MuJS 编进来；
  它与 Mermaid 用的 rquickjs 同名 `js_malloc/js_free/...` 冲突，链出来的
  Mermaid 路径直接 bus error。`render-pdf` 必须
  `mupdf = { version = "0.8", default-features = false, features = ["base14-fonts"] }`。
- 相邻页 ±1 低优先级预取（rayon 不适用于 mupdf，用自建小线程池）

**文本层**：
- `PageText { spans: Vec<{text, rect, font, size}> }` 每页一次提取，全局缓存
- **搜索**：后台线程跨页子串匹配（忽略大小写 + 可选 CJK 空格折叠），结果 `(page, char_range, rect)`；`n/N` 跳转
- **高亮**：命中/选中矩形**画进 tile 光栅**（render 时叠加半透明矩形，PNG 编码前），不依赖终端叠加能力
- **链接**：鼠标点击 → cell 坐标 → 页坐标 → 命中 link rect → 内部跳页或 `open`（macOS）
- **复制**：SGR 1006 鼠标 + PageText 命中做字符级选择（选择高亮同样画进 tile），`y` → OSC52 剪贴板
- **大纲**：`t` 弹出目录浮层（文本行，回车跳转）

## 6. 并发模型

```
主线程          渲染线程(mupdf 专属)     raster 池(rayon)        watcher 线程
事件循环          PDF tile job            md: 标题/Mermaid/图      notify + 15ms 防抖
输入/resize      text/outline 提取       img: 解码/缩放/PNG      文件变更事件
滚动/缩放             ↑ job 队列(channel)      ↑ job 队列              ↓
画笔 diff ←───────────┴─────── 结果(channel, 携带 media_id) ←──── 重布局请求
```

- 所有终端写只在主线程（画笔独占 stdout）
- Engine 不持有终端句柄 → `docp bench` 与未来 `docp serve`（daemon，M4+）复用同一引擎

## 7. 缓存

| 缓存 | 键 | 容量 | 淘汰 |
|---|---|---|---|
| md 媒体 | xxhash(内容+主题+尺寸) | ~64 项 | LRU，删除终端侧数据 |
| PDF tile | (gen, page, tile, px_w, cell_h, x) | 32 tile / 48M px | LRU（可见 tile 不淘汰）；placement 离开视口即删，图像数据保留 |
| PDF 文本层 | page | 全书（纯文本很小） | 不淘汰 |
| 终端侧图片 | image id | kitty 配额内 | 主动 a=d |

磁盘缓存（mdv 式重开加速）M3 再议，不进首版。

## 8. 交互（tkmp 键位 + PDF 扩展）

```
j/k ↑↓ 滚动          space/b 翻页/半页       d/u 半页
g/G 首/尾(G=跟随尾)   [/]  md:上/下标题  pdf:上/下页
n/p  pdf:下/上一页    1-9  跳到 10%..90%    z/Z 缩放(pdf)
h/l  pdf:水平平移(缩放后)   option+j/k pdf:5 行瞬时跳转(不平滑)
/   搜索   n/N 次匹配  t  目录(pdf)         o  打开链接
y   复制选择(OSC52)    r  强制重载           ctrl-r 重绘
+/-  md: 内容宽度      q  退出               ?  帮助浮层
鼠标: 滚轮/拖选/点击链接
```

## 9. watch / 跟随

- **markdown**：notify(debounce 15ms) → 重解析 → 块哈希 diff → 仅变化的媒体重光栅；滚动锚定（上方行数不变则不动），`G` 开启 tail 跟随
- **PDF**：inode/mtime 变化（latex 重编译）→ 后台重开文档 → 保持 `页码+页高比例`（页数变化则夹紧，缩放和水平位置不变）→ 无感刷新；实现见 `pagedview.rs`，`gen` 世代号丢弃旧文档的在途 tile
- **图片**：同 md 锚定策略

## 10. 降级链与 CLI

```
kitty graphics(U=1) → （无图形 → 纯文本模式）
md: SGR 富文本   pdf: 文本层分页器   img: 半块字符(M2)
非 TTY stdout → plain cat 模式（md 纯文本 / pdf 文本 / img 跳过）
```

```
docp <file>                 # TUI
docp <file> --page 5        # PDF 起始页
docp <file> --cat           # 不占屏，直接输出（可接 less）
docp <file> --no-graphics   # 强制纯文本
docp bench <file>           # headless 计时
```

## 11. 关键时序

**启动**：探测(≤3ms) → alt-screen → 打开文档（首屏数据优先，其余后台）→ 首帧 → 事件循环。
**滚动**：输入 → 视口计算 → 行 diff → [BSU]文本/placeholder[ESU]（无像素传输，<8ms）。
**resize**：`a=p,U=1` 全部立即重采样 → 80ms 防抖后重光栅重传替换。
**md 保存**：notify → 15ms 防抖 → diff 布局 → rayon 光栅变化块 → 传输 → 锚定滚动（目标 <30ms）。
**PDF 翻页**：预取命中则 <8ms（placeholder 重排）；未命中 → tile job（10–20ms）。

## 12. 风险与未决

| 风险 | 缓解 |
|---|---|
| Rex 对 `t=f` 文件传输、`a=f` 动画帧支持未知 | 启动特性探测，全部有 `t=d`/无动画 fallback |
| mupdf 单线程渲染成为瓶颈 | 先测（2170×2048 tile 10–15 ms，够用）；必要时多 Document 实例分片 |
| mupdf 自带 MuJS 与 rquickjs 符号冲突 | 关闭 `mupdf` 默认 feature（`js`），见 §5.3；Mermaid 回归验证 |
| 亚 cell 滚动依赖终端支持 placement 源裁剪/原位覆盖 | Rex（libghostty）实测 178×86、cell 14×32 可用；不支持时退回 cell 级滚动 |
| Rex 先给新 pane 一个 80×24 / 8×16 的占位尺寸，随后才布局成真实尺寸；cell 探测早于布局时，tile 像素纵横比与 placement 不一致，终端按比例缩放会 letterbox（页中间出现一条背景灰带） | `caps::settle()` 在进入界面前等 winsize 稳定并重探 `CSI 16t`；运行中 resize 防抖后由唯一的 tty 读线程 `input::Input::cell()` 重探（避免与键盘读取抢应答），imgview 在主线程直接重探 |
| 大页深缩放 tile 数量爆炸 | 固定 64 行 tile + 像素预算淘汰；只光栅化可见矩形 |
| CJK 在 SGR 文本路径的宽度歧义（emoji/组合字） | unicode-width + 实测校正表；文本路径可复制优先于完美对齐 |
| mupdf-sys 构建慢（首次 ~2min） | 接受；CI 缓存 |

## 13. 里程碑

- **M0 spike**（先跑通 Rex）：workspace + kgp + termui + render-img 单图显示；验证 t=f 探测
- **M1**：render-md 全量（tkmp 迁移 + syntect + 表格 CJK）+ watch + cat 模式
- **M2**（已完成）：render-pdf：fit-width 视口 tile + 翻页 + 预取 + z/Z + 水平平移；
  像素级平滑滚动；watch 重开；`--page`（图片动图顺延到 M3）
- **M3**：PDF 文本层：搜索/高亮/链接/大纲/OSC52 选择
- **M4**：纯文本降级打磨、bench、发布（cargo-brew / install.sh）
