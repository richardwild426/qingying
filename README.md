# 轻影

一个 macOS 小工具：拖入视频（单个、多个或整个文件夹），一键压小，透明通道自动保留。

原生桌面应用，用 [Native Shell](https://nsh.dev)（Rust + AppKit）写的。没有 Electron，没有 WebView。

## 用法

```sh
cargo run                # 直接跑起来
nsh run                  # 同上，nsh 会管构建
nsh package --target macos --run   # 打包成 .app 并打开
```

打开后把视频拖到中间的框里，或者点「选择文件」。压缩会自动开始。

**批量**：可以一次拖多个文件、整个文件夹，或在选择框里多选。压缩按顺序逐个进行，
界面显示「第几个 / 共几个」和总进度，完成后给出「成功几个 / 共几个」和总压缩率。

## 它做什么

源文件什么编码进来，出来的是什么：

| 源文件 | 输出 | 编码 |
|---|---|---|
| 带 alpha 通道（QTRLE、ProRes 4444、带透明的 HEVC 等） | `xxx-squeezed.mov` | HEVC with Alpha（VideoToolbox 硬件编码） |
| 普通视频（H.264、HEVC、ProRes 422 等） | `xxx-squeezed.mp4` | H.264 |

结果写回源文件所在目录，源文件不动。

### 为什么带透明要单独走一条路

MP4 / H.264 根本不存 alpha，直接把带透明的素材转 H.264 会让透明区域变黑。macOS 上能同时满足「保透明 + 剪辑软件认 + 体积小」的只有 **HEVC with Alpha**：

- ProRes 4444 也保透明，但 15 秒 1080p 就要 520 MB，等于没压
- WebM/VP9 保透明且只有 5 MB，但 QuickTime 和大多数剪辑软件不认
- HEVC Alpha 实测 860 MB → 28 MB，透明区域逐像素一致

## 画质档位

| 档位 | 无透明（H.264 CRF） | 带透明（q:v / alpha_quality） |
|---|---|---|
| 小体积 | CRF 23 | q:v 55 / 0.85 |
| 均衡（默认） | CRF 18 | q:v 50 / 1.0 |
| 高画质 | CRF 15 | q:v 40 / 1.0 |

「均衡」在视觉上基本看不出损失。带透明的素材如果边缘有羽化或阴影，别选「小体积」，alpha 会被轻微量化出毛边。



## 批量的规则

- 拖入**文件夹**时扫描它**下一层**的视频文件（不递归），避免误把整个归档目录吞进来
- 认识这些扩展名：mov mp4 m4v avi mkv webm mpg mpeg wmv flv ts mts m2ts
- 文件名带 `-squeezed` 的会被跳过，所以对同一个文件夹再压一次不会压自己的输出
- 文件夹和它里面的某个文件同时拖进来时，只压一次（按路径去重）
- 单个文件失败不会中断整批：其余文件继续，汇总里显示「N 个失败」和第一个失败原因

## 批量的进度怎么算

每个文件自己的进度会合成一个总进度：`(已完成文件数 + 当前文件的完成度) / 总数`。
所以进度条不会因为换文件而归零。

## 线程模型（重要）

NSH 在**引擎线程**上跑 UI 事件处理，而 AppKit 只允许在**主线程**创建 NSOpenPanel。
这两个限制叠加，导致两个坑：

1. `rfd::FileDialog::pick_file()`（同步版）在 NSH 的 press handler 里**必然失败**——
   它在非主线程会直接 panic。用 `AsyncFileDialog` 代替，它内部会 `run_on_main` 跳回主线程。
2. `Signal` 属于引擎线程的反应图，**worker 线程不能直接写**。后台编码线程只通过
   `mpsc` channel 发 `Update` 消息，由引擎线程上的 `set_interval` 应用。

所以 `encode()` 完全不碰 signal，进度和结果都走 channel（见 `pump()`）。



## 图标

`assets/icon.svg` 是源文件，`assets/icon.png`（1024×1024）由它渲染而来，`nsh.json` 的
`"icon"` 指向 PNG，`nsh package` 会把它转成 `AppIcon.icns` 放进 .app。

渲染方式（`inkscape` 坏了、`rsvg-convert` 没装时可用）：

```sh
qlmanage -t -s 1024 -o /tmp/iconout assets/icon.svg
cp /tmp/iconout/icon.svg.png assets/icon.png
```

图标语义：一帧胶片被两侧的箭头向内挤压 —— 「影」被压「轻」。

## 窗口尺寸

`nsh package` 的 runner 把每个应用都开成 1200×800，`nsh.json` 里没有窗口尺寸字段。
对一个单表单工具来说这是空荡荡一大片白，所以 `fit_window()` 在首帧后通过 AppKit
把窗口改成 560×620 并在屏幕上居中（`dispatch2` 派发到主队列，`NSWindow` 直接调用）。

## 布局

- 内容是一列，`max_w(500)` + `w_full`：宽窗口里不会摊开，窄窗口里会跟着缩
- 用 `items_center` + `justify_center` 让这一列在窗口里居中
- 拖放区 `flex_1` 占中间，`max_h(340)` 防止被拉成一个巨大的空框
- 底部一行：画质分段控件在左，「选择文件」在右（`flex_1()` 撑开中间）
- 结果卡片和错误提示用 `when()` 条件挂载，没结果时不占位

注意：固定宽度（如 `.w(px(500))`）在窄于该宽度的窗口里会裁掉右边的内容。
用 `w_full` + `max_w` 才跟着窗口走。


## 发布

推 `v*` 标签会触发 GitHub Actions（`.github/workflows/build.yml`）：在 Apple Silicon
runner 上构建、测试、打包、校验架构，然后产出 **DMG** 挂到该 tag 的 Release 上。

DMG 由 `.github/make-dmg.sh` 制作：里面是 app 加一个指向 `/Applications` 的链接，
窗口位置也设好了（app 在左、Applications 在右，128pt 图标），打开后把 app 拖过去就装好了。

脚本在返回前会自己验证：镜像校验和、app 和链接都在、ad-hoc 签名有效、架构是 arm64。

**版本必须与 `Cargo.toml` 里的 `nsh-*` 一致。** workflow 用 `NSH_VERSION: v0.2.0`
固定 nsh 的版本，因为 CLR 会拒绝构建依赖旧版 crate 的项目；升级 `Cargo.toml` 时
记得同步改这里。

## 依赖

- `ffmpeg` 和 `ffprobe` 在 `PATH` 里（`brew install ffmpeg`）
- macOS 14+，Xcode 命令行工具
- Rust toolchain

## 实现要点

透明检测不是看元数据（ffprobe 会把带 alpha 的 HEVC 误报成 `yuv420p`），而是真的解一帧成 RGBA，抽样统计 alpha ≠ 255 的像素比例，超过 0.5% 就判定带透明。这个判断在 `src/lib.rs` 的 `probe()` 里。

进度是从 ffmpeg 的 stderr 里抓 `time=` 算的，不是假进度条。

## 测试

```sh
nsh test
```

headless 跑 UI，验证控件和文本都在。压缩逻辑本身用 ffmpeg 直接验证过：860 MB 的 QTRLE 素材 → 30 MB，透明像素数与原片完全一致（601389/2073600）。
