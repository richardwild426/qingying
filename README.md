# 轻影

一个 macOS 小工具：拖入视频（单个、多个或整个文件夹），一键压小，透明通道自动保留。

原生桌面应用，用 [Native Shell](https://nsh.dev)（Rust + AppKit）写的。没有 Electron，没有 WebView。

## 用法

```sh
cargo run                # 直接跑起来
nsh run                  # 同上，nsh 会管构建
nsh package --target macos --run   # 打包成 .app 并打开
```

打开后把视频拖到中间的框里，或者点「选择文件」。

**选完不会立刻开始**：文件先进入「待压缩」列表，每行一个。这时可以

- 点某行的「移除」把不想压的文件拿掉
- 再拖一批进来，追加到同一个列表（同一路径只留一份）
- 点「清空」整个放弃
- 点「开始压缩」才真正开跑

**运行中可以中断**：进度条右边的「中断」会杀掉当前 ffmpeg 进程、丢弃它写了一半的输出
文件，剩下的文件不再处理。已经压完的文件保留。结果卡片会写「已中断 · 完成 N / M」，
这不是失败，所以不报错也不显示「N 个失败」。

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

鼠标悬停在三档上会显示各自的说明（这是用户唯一能看到的地方，所以文案要写人话）：

| 档位 | 无透明（H.264 CRF） | 带透明（q:v / alpha_quality） | 悬停提示 |
|---|---|---|---|
| 小体积 | CRF 23 | q:v 55 / 0.85 | 体积最小，适合发微信、传网盘；透明边缘可能有轻微毛边 |
| 均衡（默认） | CRF 18 | q:v 50 / 1.0 | 默认档，肉眼基本看不出差别；透明通道保持完整 |
| 高画质 | CRF 15 | q:v 40 / 1.0 | 最接近原片，适合还要再剪的素材；文件也最大 |

每条提示的第三行会写出对应的 ffmpeg 参数（如「无透明 CRF 15 / 带透明 q:v 40」），
方便想深究的人对照。

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

### owner：比线程更容易踩的坑

前两条是线程问题，第三条是**生命周期**问题，排查时花的时间最长：

**press handler 里创建的东西，属于它所在的那个区域。** `set_interval` 的定时器、
`signal()`、`each()` 的绑定效果，都由调用时的 `Owner` 管辖。区域被销毁（比如
`when()` 条件翻转），这些对象就一起死掉。

具体表现：`pump()` 在「开始压缩」的 handler 里调用，定时器绑在待压缩列表那个
`when` 分支上。一开始跑，列表一卸载，定时器就被销毁 —— 界面永远停在
「1 / 3 · 0%」，`[stop]` 日志打出来了但界面纹丝不动。

修法：在 App 顶层取一次 `Owner::current()`（那时还在 App 自己的作用域里），
把它传给 `pump()`，在 `owner.run(..)` 里创建定时器和 `live` 标志。

同一个陷阱的第二个表现：**`each` 不能放在会被重建的 `when` 里**。`when` 每翻一次
条件就重建一整棵子树，重建出来的 `each` 不再跟随它的信号，于是「移除」删掉了数据
但行还留在屏幕上。所以列表的 `each` 在 App 顶层构造一次，用 `.when(cond, |this| ...)`
只在需要时挂上去。



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

**版本必须与 `Cargo.toml` 里的 `nsh-*` 一致。** workflow 用 `NSH_VERSION: v0.4.0`
固定 nsh 的版本：CLI 会拒绝构建依赖不同版本 crate 的项目，并且校验装到的版本，
所以升级 `Cargo.toml` 时必须同步改这里。


## 检查更新

窗口底部显示当前版本，旁边是「检查更新」按钮。手动点击才发起请求，不后台轮询。

版本来源是 `Contents/Resources/nsh/app.json` 的 `version` 字段（`nsh package` 写入，
CI 用 `--app-version` 覆盖），所以显示的和 Finder 里看到的始终一致。

**为什么用 Atom feed 而不是 GitHub REST API**：`api.github.com` 对未认证请求限
60 次/小时/IP，共用出口 IP 时很容易被耗尽，检查就会一律失败。改用
`releases.atom`——纯 XML、无需认证、没有速率限制。解析是手写的（整个应用只读这一处
XML），取第一个 `<entry>` 的 `<title>` 作为 tag。

**但 feed 也列出只有 tag、还没有 Release 的条目**：CI 打完 tag 到上传 DMG 之间有几分钟
空窗，这时固定下载地址会是 404。所以检查更新会先对
`releases/download/<tag>/QingYing-macos-arm64.dmg` 发一个 `HEAD`；只有安装包确实存在
才显示「有新版本」，否则显示「准备中」，不给出点了必然失败的「立即更新」。

网络访问在 `nsh.json` 的 `capabilities.network.hosts` 里只授权了 `github.com` 和
`release-assets.githubusercontent.com`（下载的跳转目标），其余主机一律拒绝。

## 应用内更新

点「立即更新」后：下载 DMG → 挂载 → 把运行中的 bundle 换成新的 → 显示「已装好，重启生效」
和「重启」按钮。不用离开应用。

旧版本先被移开再复制新的，复制失败会恢复回去——半途失败不会留下不能用的 app。

**注意 `capabilities`**：`nsh.json` 里声明了 `network.hosts`（`github.com` 和
`release-assets.githubusercontent.com`，后者是 GitHub 下载的跳转目标）。但 nsh 0.2.0
**对 Rust 应用不校验这个字段**——写个非法键也不会报错，也不会生成 `capabilities.json`。
按文档，没有 capabilities 的独立应用本来就不受限，所以功能正常；这个声明是为将来
nsh 版本生效时准备的。
## 依赖

- `ffmpeg` 和 `ffprobe` 在 `PATH` 里（`brew install ffmpeg`）
- macOS 14+，Xcode 命令行工具
- Rust toolchain

## 实现要点

透明检测不是看元数据（ffprobe 会把带 alpha 的 HEVC 误报成 `yuv420p`），而是真的解一帧成 RGBA，抽样统计 alpha ≠ 255 的像素比例，超过 0.5% 就判定带透明。这个判断在 `src/lib.rs` 的 `probe()` 里。

进度是从 ffmpeg 的 stderr 里抓 `time=` 算的，不是假进度条。

## 测试

```sh
nsh test                                  # 全部：Rust 测试 + TSX 测试
cargo test                                # 只跑 Rust 的
nsh test --filter 中断                     # 名字里含「中断」的
nsh test --junit test-results.xml         # 另写 JUnit 报告（CI 用）
```

**都是 headless 的。** NSH 跑的是真引擎（同一个反应图、渲染器、组件、布局），
只是没有窗口——所以测试里能点、能拖、能等，而且不抢屏幕、不需要模拟器。
这一点很重要：靠 `open` 一个 .app 再模拟点击去验证，既慢又干扰人，还不进 CI。

### Rust 测试：`tests/updater.rs`

更新器的问题只有 Rust 侧能测到。用 `nsh_testing::Headless` + `StubNetwork` 起真 app，
把 GitHub 的 feed 和 DMG 打桩，然后**断言 app 向网络请求了什么**：

```rust
let network = Arc::new(StubNetwork::new([
    (format!("GET {FEED}"), response(200, ATOM.as_bytes())),
    (format!("GET {ASSET}"), response(200, b"not a dmg")),
]));
let mut config = EngineConfig::new(Root::of::<nsh_app_video_squeezer::App>());
config.network = Some(network.clone());
let mut app = Headless::start(config)?;
app.wait_for("text:轻影")?;        // 首帧还没渲染，必须先等一帧

app.press("text:检查更新")?;
app.wait_for("text:/有新版本/")?;
app.press("text:立即更新")?;
app.wait_until("请求资源", Duration::from_secs(10), |_| network.requests().len() >= 2)?;
```

三个断言：

1. 检查更新只请求 feed
2. **点「立即更新」必须真的发出资源请求**（这条抓的就是「下载卡在 0」——修复前
   `network.requests()` 只有 1 条，任务在被按下的按钮所在的 region 销毁时就被取消了）
3. 状态必须离开 0（不能停在 `下载中 0.0 MB`）

**验证测试本身有效**：用修复前的 `src/lib.rs` 跑这三个测试，第 2、3 条失败，
屏幕内容正是用户看到的 `下载中 0.0 MB`。测试确实复现了那个 bug。

### TSX 测试：`App.test.tsx`

UI 交互在 TSX 里写，选择器与 Rust 侧一致：

- **画质提示**：`await app.hover("id:quality-high")` 真的悬停等 tooltip，断言含
  「最接近原片」「CRF 15」。tooltip 需要指针，必须 `open(".", { platform: "macos" })`，
  默认 iOS 会报 "a phone has no mouse"。
- **待压缩列表**：`await app.dropFiles("id:dropzone", [uris])` 真的拖文件。
- **移除 / 清空**：断言文件名从屏幕上消失（这条抓到过 `each` 不跟随信号的 bug）。
- **中断**：开始压缩后点「中断」，断言出现「已中断」且**没有**「个失败」。
- **正常跑完**：不中断时给出结果卡片和打开按钮。

`hover` / `wait` 是 async 要 `await`；没有 `waitFor(ms)`，是 `wait(ms)`；
选择器多匹配要 `>> nth=0`。

测试素材 `.test-fixtures/clip{1,2,3}.mp4` 由 CI 用 ffmpeg 现场生成（720p / 12 秒，
够长才能被打断），不入库。本地跑之前先生成一次，命令见 workflow 的
「Make the test clips」。

### 测试里不要做的事

- **不要 `open` 打包好的 .app 再模拟点击**。那是手工验证，不是测试：它抢前台、
  依赖屏幕分辨率、点偏了会点到别的窗口，而且 CI 跑不了。headless 全都能做。
- **不要用子进程（`curl` 等）访问网络**。它绕过 `capabilities`，也让
  `StubNetwork` 看不见——「下载没发生」和「下载绕过了沙箱」在测试里长得一样。
  这也是更新器改回 `fetch` 的原因。
- **不要在 `Headless::start` 之后立刻 `press`**。首帧还没渲染，屏幕是空的；
  先 `wait_for` 一个必然存在的文本。

压缩逻辑本身用 ffmpeg 直接验证过：860 MB 的 QTRLE 素材 → 30 MB，透明像素数与原片完全一致（601389/2073600）。

中断的清理也单独验证过：`kill -9` 一个跑到一半的 ffmpeg 确实会留下半截 mp4，
所以 `encode_one()` 在中断分支里 `remove_file`，测试跑完后 fixture 目录里没有残留。
