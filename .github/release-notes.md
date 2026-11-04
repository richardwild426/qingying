### macOS (Apple Silicon)

Download `QingYing-macos-arm64.zip`, unzip it, and drag 轻影 into Applications.

The app is ad hoc signed, not notarized, so macOS quarantines a build that was
downloaded. Clear it once after unzipping:

```sh
xattr -dr com.apple.quarantine "/Applications/轻影.app"
```

The app needs macOS 14 or later on an M-series Mac, and `ffmpeg`
(`brew install ffmpeg`).
