// The Rust root tested from TypeScript: `nsh test` runs this file in a
// headless runner it builds with the crate, and `open(".")` starts the root
// (`nsh.json` "root").
import { expect, open, test } from "nsh/test";

test("shows the drop zone and the controls", async () => {
  const app = await open(".");
  const texts = app.texts();
  expect(texts).toContain("轻影");
  expect(texts).toContain("拖到窗口任意位置");
  // the drop target says a folder works too
  expect(texts.join(" ")).toContain("文件夹");
  expect(texts).toContain("选择文件");
  // every quality preset is offered
  expect(texts).toContain("小体积");
  expect(texts).toContain("均衡");
  expect(texts).toContain("高画质");
});

test("switches the quality preset", async () => {
  const app = await open(".");
  await app.press("高画质");
  // the press is accepted and the app still shows its drop zone
  expect(app.texts()).toContain("拖到窗口任意位置");
});

// Each preset says what it means. The hint is a tooltip: absent from the
// window until the pointer rests on the preset, so hovering is the only
// honest way to test that it is reachable at all.
test("hovering a preset explains what it does", async () => {
  // A tooltip needs a pointer, which only the desktops have.
  const app = await open(".", { platform: "macos" });
  expect(app.texts().join(" ")).not.toContain("CRF");

  await app.hover("id:quality-high");
  await app.wait(600);

  const hint = app.texts().join(" ");
  expect(hint).toContain("最接近原片");
  // the flag it maps to is part of the explanation
  expect(hint).toContain("CRF 15");
});

test("the small preset warns about alpha edges", async () => {
  const app = await open(".", { platform: "macos" });
  await app.hover("id:quality-small");
  await app.wait(600);
  const hint = app.texts().join(" ");
  expect(hint).toContain("体积最小");
  expect(hint).toContain("毛边");
});

// Dropping files no longer starts encoding: the files wait in a list, which
// is what makes "cancel a file I picked" possible at all. The clips come from
// .test-fixtures (see the CI step that makes them).
const clip = (n: number) =>
  new URL(`.test-fixtures/clip${n}.mp4`, import.meta.url).href;

test("dropped files wait in a list instead of encoding at once", async () => {
  const app = await open(".", { platform: "macos" });
  await app.dropFiles("id:dropzone", [clip(1), clip(2)]);
  await app.waitFor("text:2 个文件待压缩");

  const texts = app.texts();
  expect(texts).toContain("clip1.mp4");
  expect(texts).toContain("clip2.mp4");
  // nothing is running: the run is the user's to start
  expect(texts).toContain("开始压缩");
  expect(texts.join(" ")).not.toContain("HEVC + 透明");
});

// The whole window is the drop target, not just the box in the middle.
test("a file dropped anywhere in the window is accepted", async () => {
  const app = await open(".", { platform: "macos" });
  await app.dropFiles("id:app-root", [clip(1)]);
  await app.waitFor("text:1 个文件待压缩");
});

test("a file can be taken back out before the run starts", async () => {
  const app = await open(".", { platform: "macos" });
  await app.dropFiles("id:dropzone", [clip(1), clip(2)]);
  await app.waitFor("text:2 个文件待压缩");

  // the first row's remove button
  await app.press("text:移除 >> nth=0");
  await app.waitFor("text:1 个文件待压缩");

  const texts = app.texts();
  expect(texts).not.toContain("clip1.mp4");
  expect(texts).toContain("clip2.mp4");
});

test("clearing the list goes back to the empty window", async () => {
  const app = await open(".", { platform: "macos" });
  await app.dropFiles("id:dropzone", [clip(1)]);
  await app.waitFor("text:1 个文件待压缩");

  await app.press("清空");
  await app.waitFor("text:拖到窗口任意位置");

  // the list's controls are gone with it
  expect(app.texts()).not.toContain("开始压缩");
});

test("a run can be stopped while it encodes", async () => {
  const app = await open(".", { platform: "macos" });
  await app.dropFiles("id:dropzone", [clip(1), clip(2), clip(3)]);
  await app.waitFor("text:3 个文件待压缩");

  await app.press("开始压缩");
  // The stop is only offered while a file is encoding, which is the window
  // this test needs anyway.
  await app.waitFor("text:中断");

  await app.press("中断");
  await app.waitFor("text:/已中断/");

  const texts = app.texts().join(" ");
  expect(texts).toContain("已中断");
  // stopping is not failing: no failure is reported, and the rest of the
  // batch did not run
  expect(texts).not.toContain("个失败");
});

// The other half of the same coin: a run left alone finishes, and says how
// much it saved rather than that it was stopped.
test("a run that is not stopped finishes with its tallies", async () => {
  const app = await open(".", { platform: "macos" });
  await app.dropFiles("id:dropzone", [clip(1)]);
  await app.waitFor("text:1 个文件待压缩");

  await app.press("开始压缩");
  await app.waitFor("text:在访达中显示", { timeout: 60000 });

  const texts = app.texts().join(" ");
  expect(texts).not.toContain("已中断");
  // the result card offers the output
  expect(texts).toContain("打开");
});

// The picker opens a native panel, which a headless run has no window for.
// What is tested here is that the press reaches the handler and the app
// survives it: if the handler trapped, this would fail.
test("the pick button is pressable", async () => {
  const app = await open(".");
  await app.press("选择文件");
  expect(app.texts()).toContain("拖到窗口任意位置");
});

// The result card only exists once a file has been encoded, so it is absent
// from a fresh window — and, just as importantly, its controls do not sit in
// the layout waiting to be pressed.
test("no result controls before a run", async () => {
  const app = await open(".");
  const texts = app.texts();
  expect(texts).not.toContain("在访达中显示");
  expect(texts).not.toContain("打开");
});

// The updater's controls are in the window from the start; the check itself
// is manual, so nothing is fetched until the user asks.
test("offers a manual update check", async () => {
  const app = await open(".");
  const texts = app.texts();
  expect(texts).toContain("检查更新");
  expect(texts.join(" ")).toMatch(/轻影 \d+\.\d+\.\d+/);
});
