// The Rust root tested from TypeScript: `nsh test` runs this file in a
// headless runner it builds with the crate, and `open(".")` starts the root
// (`nsh.json` "root").
import { expect, open, test } from "nsh/test";

test("shows the drop zone and the controls", async () => {
  const app = await open(".");
  const texts = app.texts();
  expect(texts).toContain("轻影");
  expect(texts).toContain("把视频拖到这里");
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
  expect(app.texts()).toContain("把视频拖到这里");
});

// The picker opens a native panel, which a headless run has no window for.
// What is tested here is that the press reaches the handler and the app
// survives it: if the handler trapped, this would fail.
test("the pick button is pressable", async () => {
  const app = await open(".");
  await app.press("选择文件");
  expect(app.texts()).toContain("把视频拖到这里");
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
