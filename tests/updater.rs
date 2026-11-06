//! The updater, driven the way NSH's testing doc describes: a headless
//! engine, a stubbed network, and the same selectors the TSX tests use.
//!
//! This exists because the updater broke in a way the TSX tests could not
//! see: the download was spawned from a button's press handler, so it
//! belonged to that button's region and was cancelled the moment the state
//! changed. Nothing reached the network at all. Asserting on what the app
//! *asked the network for* catches that; asserting on what the window said
//! did not.
//!
//! `cargo test` runs these, and so does `nsh test` (it runs each crate's
//! `cargo test` before the TSX files).

use std::sync::Arc;
use std::time::Duration;

use nsh_engine::{EngineConfig, Root};
use nsh_testing::{Headless, StubNetwork, StubResponse};

/// What GitHub's release feed answers: a version newer than this build.
const FEED: &str = "https://github.com/richardwild426/qingying/releases.atom";
const ASSET: &str =
    "https://github.com/richardwild426/qingying/releases/download/v99.0.0/QingYing-macos-arm64.dmg";

const ATOM: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <entry>
    <title>v99.0.0</title>
    <link href="https://github.com/richardwild426/qingying/releases/tag/v99.0.0"/>
  </entry>
</feed>"#;

fn response(status: u16, body: &[u8]) -> StubResponse {
    // `StubResponse` is `#[non_exhaustive]`: fields are set, not built.
    let mut r = StubResponse::default();
    r.status = status;
    r.body = body.to_vec();
    r
}

/// An app whose network answers the feed and the asset, and which records
/// every request it made.
fn app() -> (Headless, Arc<StubNetwork>) {
    let network = Arc::new(StubNetwork::new([
        (format!("GET {FEED}"), response(200, ATOM.as_bytes())),
        // Not a real DMG: what matters is that the app asks for it.
        (format!("GET {ASSET}"), response(200, b"not a dmg")),
    ]));
    let mut config = EngineConfig::new(Root::of::<nsh_app_video_squeezer::App>());
    config.network = Some(network.clone());
    let mut app = Headless::start(config).expect("the app starts headless");
    // Let the first frame render: `start` returns as the engine is launched.
    app.wait_for("text:轻影").expect("the window renders");
    (app, network)
}

#[test]
fn checking_for_updates_fetches_the_release_feed() {
    let (mut app, network) = app();
    app.press("text:检查更新")
        .expect("the check button is there");
    app.wait_for("text:/有新版本/")
        .expect("a newer release is found");

    let asked: Vec<String> = network.requests().iter().map(|r| r.url.clone()).collect();
    assert_eq!(asked, [FEED], "the check asks GitHub's feed, and only it");
}

/// The regression: pressing 立即更新 must reach the network. Before the fix
/// the download task was cancelled before it ran, so this list stayed empty
/// and the window sat at "下载中 0.0 MB" forever.
#[test]
fn starting_the_download_asks_for_the_asset() {
    let (mut app, network) = app();
    app.press("text:检查更新").expect("check");
    app.wait_for("text:/有新版本/").expect("found");

    app.press("text:立即更新")
        .expect("the update button is there");
    // The download runs on a worker thread; the request must show up.
    app.wait_until("the asset is requested", Duration::from_secs(10), |_| {
        network.requests().len() >= 2
    })
    .expect("the asset is requested, not silently skipped");

    let asked: Vec<String> = network.requests().iter().map(|r| r.url.clone()).collect();
    assert_eq!(
        asked,
        [FEED, ASSET],
        "the feed, then the release asset it named"
    );
}

/// The window has to say something happened, whichever way the download
/// ends — it must not stay on the zero it used to be stuck at.
#[test]
fn the_download_leaves_the_zero_it_used_to_sit_on() {
    let (mut app, _network) = app();
    app.press("text:检查更新").expect("check");
    app.wait_for("text:/有新版本/").expect("found");

    app.press("text:立即更新").expect("update");
    // Stub bytes are not a disk image, so the install fails — but it fails
    // *after* the download, which is the point.
    app.wait_for("text:/下载中|已装好|失败|已更新/")
        .expect("the state moved off the zero");
    app.wait(Duration::from_millis(500))
        .expect("the app keeps running");
    assert!(
        !app.shows("下载中 0.0 MB"),
        "still at zero after the download: {}",
        app.screen().texts().join(" | ")
    );
}
