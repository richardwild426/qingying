### 修复

- **应用内更新现在真的能下载了**。之前点「立即更新」会一直停在「下载中 0.0 MB」，
  既不报错也不前进。原因有两个：下载任务绑在被按下的那个按钮上，按钮一换状态任务
  就被取消了；而且进度数字只在下载完成后才写一次，过程中永远是 0。现在任务不再随
  按钮消失，下载期间显示「正在下载…」而不是一个不会动的 0。

### 新功能

- **选完先确认**：拖入或选中的文件先进「待压缩」列表，可以逐个「移除」、再追加、
  或「清空」，点「开始压缩」才真正开跑。
- **可以中断**：压缩途中点「中断」会停掉当前文件（写了一半的输出会被删掉），
  已完成的保留，结果显示「已中断 · 完成 N / M」。
- **画质档位有说明了**：鼠标悬停在小体积 / 均衡 / 高画质上，会显示各自适合什么场景、
  代价是什么，以及对应的 ffmpeg 参数。

### 安装

Download `QingYing-macos-arm64.dmg`, open it, and drag 轻影 onto the
Applications folder in the window that appears.

The app is ad hoc signed, not notarized, so macOS quarantines a build that was
downloaded. If it refuses to open the first time, clear the flag once:

```sh
xattr -dr com.apple.quarantine "/Applications/轻影.app"
```

The app needs macOS 14 or later on an M-series Mac, and `ffmpeg`
(`brew install ffmpeg`).
