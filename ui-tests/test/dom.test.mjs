//! 设置页 DOM 交互测试：加载真实 settings.html，模拟 IPC（window.ipc），
//! 验证按钮动作、状态渲染、隐藏页结果、错误提示与键盘输入映射。
//! 运行：npm ci --prefix ui-tests && npm test --prefix ui-tests

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { JSDOM } from "jsdom";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const html = readFileSync(join(root, "..", "apps", "tray", "src", "settings.html"), "utf8");

/**
 * 创建加载 settings.html 的 DOM。
 * window.ipc.postMessage 被捕获到 outbox；返回 dom 与 outbox。
 */
function createDom() {
  const outbox = [];
  const dom = new JSDOM(html, {
    runScripts: "dangerously",
    url: "http://localhost/settings.html",
    pretendToBeVisual: true,
    beforeParse(window) {
      window.ipc = { postMessage: (msg) => outbox.push(JSON.parse(msg)) };
      // jsdom 未实现 scrollTo 等，防告警。
      window.scrollTo = () => {};
    },
  });
  return { dom, outbox };
}

const sampleState = {
  version: "0.8.0",
  mode: "hybrid",
  recording: false,
  hotkey: "F9",
  hotkeyVk: 0x78,
  hotkeyMods: 0,
  autostart: false,
  pending: [],
  lastCheck: null,
  lastAnnouncement: null,
  modelReady: true,
  modelStatus: "模型已就绪（当前版本）：C:\\models\\sub",
  modelPath: "C:\\models\\sub",
};

test("加载后发送 ready 动作", () => {
  const { outbox } = createDom();
  assert.deepEqual(outbox, [{ action: "ready" }]);
});

test("state() 渲染版本/模式/模型状态/快捷键", () => {
  const { dom } = createDom();
  const { window } = dom;
  window.voiceType.state({ ...sampleState });
  const doc = window.document;
  assert.equal(doc.getElementById("version").textContent, "v0.8.0");
  assert.equal(doc.getElementById("status-title").textContent, "准备就绪");
  assert.equal(doc.getElementById("model-desc").textContent, sampleState.modelStatus);
  assert.equal(doc.getElementById("model-path").textContent, sampleState.modelPath);
  assert.equal(doc.getElementById("hotkey").value, "F9");
  assert.equal(doc.getElementById("autostart").classList.contains("on"), false);
  const activeMode = doc.querySelector("[data-mode].active");
  assert.equal(activeMode.dataset.mode, "hybrid");
});

test("模型未就绪时显示后端状态文本并禁用录音按钮", () => {
  const { dom } = createDom();
  const { window } = dom;
  window.voiceType.state({
    ...sampleState,
    modelReady: false,
    modelStatus: "模型损坏：C:\\x\\encoder.onnx（SHA-256 不匹配）",
  });
  const doc = window.document;
  assert.ok(doc.getElementById("status-title").textContent.includes("模型损坏"));
  assert.equal(doc.getElementById("record-btn").disabled, true);
  assert.equal(doc.getElementById("model-badge").textContent, "异常");
});

test("点击模式按钮发送带 seq 的 mode 动作，进行中防重复", () => {
  const { dom, outbox } = createDom();
  const { window } = dom;
  const button = window.document.querySelector('[data-mode="paste"]');
  button.click();
  button.click(); // 进行中重复点击不应产生第二条消息
  assert.equal(outbox.length, 2); // ready + 一条 mode
  const mode = outbox[1];
  assert.equal(mode.action, "mode");
  assert.equal(mode.value, "paste");
  assert.equal(typeof mode.seq, "number");
  assert.equal(button.disabled, true, "提交后按钮应禁用");
});

test("result(running) 保存提示并保持禁用；result(done) 解禁并提示", () => {
  const { dom, outbox } = createDom();
  const { window } = dom;
  const doc = window.document;
  doc.getElementById("update-btn").click();
  const seq = outbox.at(-1).seq;
  window.voiceType.result({ seq, action: "checkUpdate", state: "running", message: "正在检查更新…" });
  assert.equal(doc.getElementById("update-btn").disabled, true);
  assert.ok(doc.getElementById("notice").textContent.includes("正在检查更新"));
  window.voiceType.result({ seq, action: "checkUpdate", state: "done", ok: true, message: "已是最新版本" });
  assert.equal(doc.getElementById("update-btn").disabled, false);
  assert.ok(doc.getElementById("notice").textContent.includes("已是最新版本"));
});

test("过期 seq 的结果被忽略", () => {
  const { dom, outbox } = createDom();
  const { window } = dom;
  const doc = window.document;
  doc.getElementById("repair-models").click();
  const staleSeq = outbox.at(-1).seq - 999;
  window.voiceType.result({ seq: staleSeq, action: "repairModels", state: "done", ok: true, message: "过期" });
  assert.equal(doc.getElementById("repair-models").disabled, true, "过期结果不得解禁按钮");
});

test("录音按钮发送 record 动作（不直接注入，由后端隐藏窗口）", () => {
  const { dom, outbox } = createDom();
  const { window } = dom;
  window.voiceType.state({ ...sampleState });
  window.document.getElementById("record-btn").click();
  const msg = outbox.at(-1);
  assert.equal(msg.action, "record");
  assert.equal(typeof msg.seq, "number");
});

test("热键捕获：event.code 映射为 VK（Ctrl+F9 → vk 0x78 mods 2）", () => {
  const { dom, outbox } = createDom();
  const { window } = dom;
  const doc = window.document;
  const input = doc.getElementById("hotkey");
  input.dispatchEvent(new window.KeyboardEvent("focus"));
  input.dispatchEvent(new window.KeyboardEvent("keydown", {
    code: "F9", key: "F9", ctrlKey: true, cancelable: true, bubbles: true,
  }));
  assert.equal(input.value, "Ctrl + F9");
  doc.getElementById("apply-hotkey").click();
  const msg = outbox.at(-1);
  assert.equal(msg.action, "hotkey");
  assert.equal(msg.vk, 0x78);
  assert.equal(msg.mods, 0x2);
});

test("热键捕获：字母 A → vk 0x41；纯修饰键不入待定", () => {
  const { dom, outbox } = createDom();
  const { window } = dom;
  const doc = window.document;
  const input = doc.getElementById("hotkey");
  input.dispatchEvent(new window.KeyboardEvent("keydown", {
    code: "KeyA", key: "a", altKey: true, cancelable: true, bubbles: true,
  }));
  doc.getElementById("apply-hotkey").click();
  const msg = outbox.at(-1);
  assert.equal(msg.action, "hotkey");
  assert.equal(msg.vk, 0x41);
  assert.equal(msg.mods, 0x1);
  // 纯 Shift 按键被忽略：不产生待定组合，显示值保持不变
  outbox.length = 0;
  input.dispatchEvent(new window.KeyboardEvent("keydown", {
    code: "ShiftLeft", key: "Shift", shiftKey: true, cancelable: true, bubbles: true,
  }));
  assert.equal(input.value, "Alt + A", "纯修饰键不得改变待定组合");
  // 无待定时点击应用不发消息
  doc.getElementById("apply-hotkey").click();
  assert.equal(outbox.filter(m => m.action === "hotkey").length, 0);
});

test("热键捕获：不支持 Win 键与未知键位", () => {
  const { dom } = createDom();
  const { window } = dom;
  const doc = window.document;
  const input = doc.getElementById("hotkey");
  input.dispatchEvent(new window.KeyboardEvent("keydown", {
    code: "KeyA", key: "a", metaKey: true, cancelable: true, bubbles: true,
  }));
  assert.ok(input.value.includes("不支持 Win"));
  input.dispatchEvent(new window.KeyboardEvent("keydown", {
    code: "SomeUnknownKey", key: "Unk", cancelable: true, bubbles: true,
  }));
  assert.ok(input.value.includes("不支持"));
});

test("自启开关发送布尔值", () => {
  const { dom, outbox } = createDom();
  const { window } = dom;
  window.voiceType.state({ ...sampleState });
  window.document.getElementById("autostart").click();
  const msg = outbox.at(-1);
  assert.equal(msg.action, "autostart");
  assert.equal(msg.value, true);
});

test("C1：lastCheck/lastAnnouncement 持久渲染，首页显示公告提示", () => {
  const { dom } = createDom();
  const { window } = dom;
  const doc = window.document;
  window.voiceType.state({
    ...sampleState,
    lastCheck: { ok: false, text: "下载更新失败：网络超时" },
    lastAnnouncement: { ok: true, text: "0.8.0 发布\n【新增：…】" },
  });
  const result = doc.getElementById("update-result").textContent;
  assert.ok(result.includes("失败"));
  assert.ok(result.includes("网络超时"));
  const announcement = doc.getElementById("announcement").textContent;
  assert.ok(announcement.includes("0.8.0 发布"));
  const hint = doc.getElementById("announce-hint");
  assert.equal(hint.hidden, false);
  assert.ok(hint.textContent.includes("公告已更新"));
  // 首行摘要出现在首页提示
  assert.ok(hint.textContent.includes("0.8.0 发布"));
});

test("C1：无持久结果时显示占位并隐藏首页提示", () => {
  const { dom } = createDom();
  const { window } = dom;
  window.voiceType.state({ ...sampleState });
  assert.equal(window.document.getElementById("update-result").textContent, "尚无检查结果。");
  assert.equal(window.document.getElementById("announce-hint").hidden, true);
});

test("状态推送恢复进行中按钮的禁用态（重开窗口场景）", () => {
  const { dom } = createDom();
  const { window } = dom;
  window.voiceType.state({ ...sampleState, pending: ["repairModels"] });
  assert.equal(window.document.getElementById("repair-models").disabled, true);
  window.voiceType.state({ ...sampleState, pending: [] });
  assert.equal(window.document.getElementById("repair-models").disabled, false);
});

test("动态文本一律走 textContent（无 innerHTML 注入路径）", () => {
  // 静态保障：脚本内不允许出现 innerHTML
  assert.ok(!/<script>[\s\S]*innerHTML/.test(html), "settings.html 脚本不得使用 innerHTML");
});
