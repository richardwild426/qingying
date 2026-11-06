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

/// An app whose network answers the feed, the asset HEAD check and the
/// download, and which records every request it made.
fn app_with_asset(head_status: u16, get_status: u16) -> (Headless, Arc<StubNetwork>) {
    let network = Arc::new(StubNetwork::new([
        (format!("GET {FEED}"), response(200, ATOM.as_bytes())),
        (format!("HEAD {ASSET}"), response(head_status, b"")),
        // Not a real DMG: what matters is that the app asks for it.
        (format!("GET {ASSET}"), response(get_status, b"not a dmg")),
    ]));
    let mut config = EngineConfig::new(Root::of::<nsh_app_video_squeezer::App>());
    config.network = Some(network.clone());
    let mut app = Headless::start(config).expect("the app starts headless");
    // Let the first frame render: `start` returns as the engine is launched.
    app.wait_for("text:轻影").expect("the window renders");
    (app, network)
}

fn app() -> (Headless, Arc<StubNetwork>) {
    app_with_asset(200, 200)
}

fn asked(network: &StubNetwork) -> Vec<String> {
    network
        .requests()
        .iter()
        .map(|r| format!("{} {}", r.method, r.url))
        .collect()
}

#[test]
fn checking_for_updates_fetches_the_feed_and_verifies_the_asset() {
    let (mut app, network) = app();
    app.press("text:检查更新")
        .expect("the check button is there");
    app.wait_for("text:/有新版本/")
        .expect("a newer release is found");

    assert_eq!(
        asked(&network),
        [format!("GET {FEED}"), format!("HEAD {ASSET}")],
        "the check asks for the feed, then verifies the DMG is published"
    );
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
        network.requests().len() >= 3
    })
    .expect("the asset is requested, not silently skipped");

    assert_eq!(
        asked(&network),
        [
            format!("GET {FEED}"),
            format!("HEAD {ASSET}"),
            format!("GET {ASSET}")
        ],
        "the feed, the asset check, then the release asset it named"
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

/// `releases.atom` lists a tag as soon as it is pushed, minutes before CI
/// uploads the DMG. Offering that as an update only produces a 404.
#[test]
fn a_tag_without_a_published_asset_is_not_offered_as_an_update() {
    let (mut app, network) = app_with_asset(404, 200);
    app.press("text:检查更新").expect("check");
    app.wait_for("text:/准备中/")
        .expect("the tag is still being prepared");

    let texts = app.screen().texts().join(" | ");
    assert!(!texts.contains("有新版本"), "offered a 404: {texts}");
    assert!(!texts.contains("立即更新"), "offered a 404: {texts}");
    assert!(
        texts.contains("安装包还没上传"),
        "no explanation for the wait: {texts}"
    );
    assert_eq!(
        asked(&network),
        [format!("GET {FEED}"), format!("HEAD {ASSET}")],
        "the check stops at the missing asset"
    );
}

/// Even with the HEAD check, the asset can disappear between check and
/// download; the user should see why, not a bare HTTP code.
#[test]
fn a_download_that_404s_explains_the_missing_asset() {
    let (mut app, _network) = app_with_asset(200, 404);
    app.press("text:检查更新").expect("check");
    app.wait_for("text:/有新版本/").expect("found");

    app.press("text:立即更新").expect("update");
    app.wait_for("text:/安装包还没上传完成/")
        .expect("the 404 is explained");
    assert!(!app.shows("HTTP 404"), "showed a bare status code");
}
