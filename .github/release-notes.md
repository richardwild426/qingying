### macOS (Apple Silicon)

Download `QingYing-macos-arm64.dmg`, open it, and drag 轻影 onto the
Applications folder in the window that appears.

The app is ad hoc signed, not notarized, so macOS quarantines a build that was
downloaded. If it refuses to open the first time, clear the flag once:

```sh
xattr -dr com.apple.quarantine "/Applications/轻影.app"
```

The app needs macOS 14 or later on an M-series Mac, and `ffmpeg`
(`brew install ffmpeg`).
