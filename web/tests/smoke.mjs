// 仅启动临时数据库与本地假上游；不连接用户的部署或真实供应商。
import assert from "node:assert/strict";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createServer } from "node:http";
import { spawn } from "node:child_process";
import { chromium } from "playwright-core";

const directory = await mkdtemp(join(tmpdir(), "akhub-ui-review-"));
const upstream = createServer((req, res) => {
  res.setHeader("content-type", "application/json");
  res.end(JSON.stringify(req.url === "/v1/models"
    ? { data: [{ id: "review-model" }] }
    : { id: "chat-review", choices: [{ message: { role: "assistant", content: "ok" }, finish_reason: "stop" }], usage: { prompt_tokens: 10, completion_tokens: 2 } }));
});
await new Promise((done) => upstream.listen(0, "127.0.0.1", done));
const allocator = createServer();
await new Promise((done) => allocator.listen(0, "127.0.0.1", done));
const port = allocator.address().port;
await new Promise((done) => allocator.close(done));
const base = `http://127.0.0.1:${port}`;
const child = spawn(resolve(process.env.AKHUB_BINARY ?? "../target/debug/akhub"), [], {
  env: { ...process.env, AKHUB_DATA_DIR: join(directory, "data"), AKHUB_LISTEN: `127.0.0.1:${port}` },
  stdio: "ignore",
});
let browser;
try {
  for (let i = 0; i < 100; i++) {
    if (child.exitCode !== null) throw new Error("测试服务提前退出");
    if (await fetch(`${base}/health/ready`).then(r => r.ok, () => false)) break;
    await new Promise(done => setTimeout(done, 100));
  }
  browser = await chromium.launch({ headless: true, executablePath: process.env.CHROME_PATH });
  const context = await browser.newContext({ viewport: { width: 1440, height: 1000 } });
  const page = await context.newPage();
  page.setDefaultTimeout(10000);
  const errors = [];
  page.on("pageerror", error => errors.push(error.message));
  await page.goto(`${base}/admin`);
  await page.getByLabel("密码", { exact: true }).fill("review-only-password-123");
  await page.getByRole("button", { name: "创建管理员", exact: true }).click();
  await page.locator(".nav-item").first().waitFor();
  const post = async (path, data) => {
    const response = await context.request.post(`${base}/admin/api${path}`, {
      headers: { "x-akhub-csrf": "1" }, data,
    });
    assert(response.ok(), `${path}: ${response.status()} ${await response.text()}`);
    return response.json();
  };
  const group = await post("/groups", { name: "验收分组", multiplier_limit: "2" });
  const account = await post("/accounts", {
    group_id: group.group.id, name: "验收账号", base_url: `http://127.0.0.1:${upstream.address().port}`,
    api_key: "review-fake-key", preferred_protocol: "openai_chat", allow_private_network: true,
    multiplier_mode: "manual", manual_multiplier: "1",
  });
  await post(`/accounts/${account.id}/models/refresh`);
  await post(`/accounts/${account.id}/models/apply`, { changes: [{ upstream_model: "review-model", selected: true }] });
  const inference = await context.request.post(`${base}/v1/chat/completions`, {
    headers: { authorization: `Bearer ${group.key}` }, data: { model: "review-model", messages: [{ role: "user", content: "hi" }] },
  });
  assert.equal(inference.status(), 200, "测试模型必须能通过真实网关调用");
  await page.reload();
  for (const route of ["overview", "groups", "accounts", "targets", "requests", "cost", "settings"]) {
    await page.goto(`${base}/admin#/${route}`);
    await page.locator(".nav-item").first().waitFor();
    await page.locator(".skeleton").first().waitFor({ state: "hidden" });
    await page.screenshot({ path: join(directory, `${route}.png`), fullPage: true, animations: "disabled" });
    assert((await page.locator("body").innerText()).length > 100, `${route} 页面为空`);
  }
  await page.goto(`${base}/admin#/groups`);
  await page.getByRole("button", { name: "新建分组", exact: true }).first().click();
  const drawer = page.getByRole("dialog", { name: "新建分组", exact: true });
  await drawer.getByLabel("名称", { exact: true }).fill("未保存的分组");
  await page.keyboard.press("Escape");
  await page.getByRole("dialog", { name: "放弃未保存的修改？" }).waitFor();
  await page.getByRole("button", { name: "取消", exact: true }).last().click();
  assert.equal(await drawer.getByLabel("名称", { exact: true }).inputValue(), "未保存的分组");
  await page.keyboard.press("Escape");
  await page.getByRole("button", { name: "放弃修改", exact: true }).click();
  await drawer.waitFor({ state: "hidden" });
  await page.goto(`${base}/admin#/accounts`);
  await page.getByRole("button", { name: "模型管理", exact: true }).first().click();
  const manager = page.getByRole("dialog", { name: "模型管理 · 验收账号", exact: true });
  await manager.getByText("review-model", { exact: true }).first().waitFor();
  await manager.getByRole("button", { name: "已启用", exact: true }).click();
  await manager.getByRole("button", { name: "保存", exact: true }).click();
  const warning = page.getByRole("dialog", { name: "这些模型最近 24 小时有流量", exact: true });
  await warning.waitFor();
  for (let i = 0; i < 8; i++) {
    await page.keyboard.press("Tab");
    assert(await warning.evaluate(el => el.contains(document.activeElement)), "Tab 焦点只能留在顶层确认框");
  }
  await page.keyboard.press("Escape");
  await warning.waitFor({ state: "hidden" });
  await manager.getByRole("button", { name: "已启用", exact: true }).waitFor();
  assert.equal(await page.getByRole("dialog", { name: "放弃未保存的修改？" }).count(), 0,
    "Esc 只能关闭顶层确认，不能同时触发底层的丢弃草稿确认");
  assert.equal(await page.evaluate(() => document.body.style.overflow), "hidden", "底层弹窗仍在时不能解除滚动锁");
  await page.keyboard.press("Escape");
  await manager.waitFor({ state: "hidden" });
  assert.notEqual(await page.evaluate(() => document.body.style.overflow), "hidden", "关闭所有弹窗后必须恢复滚动");
  await page.goto(`${base}/admin#/targets`);
  await page.locator(".routing-model-block").first().click();
  await page.locator(".routing-table tbody tr").first().waitFor();
  await page.screenshot({ path: join(directory, "scores-expanded.png"), fullPage: true, animations: "disabled" });
  await page.locator("button.routing-account-name").first().click();
  await manager.waitFor();
  await page.keyboard.press("Escape");
  await page.getByRole("button", { name: "深色主题", exact: true }).click();
  await page.screenshot({ path: join(directory, "dark-accounts.png"), fullPage: true, animations: "disabled" });
  await page.setViewportSize({ width: 390, height: 844 });
  for (const route of ["overview", "groups", "accounts", "targets", "requests", "cost", "settings"]) {
    await page.goto(`${base}/admin#/${route}`);
    await page.locator(".skeleton").first().waitFor({ state: "hidden" });
    await page.screenshot({ path: join(directory, `mobile-${route}.png`), fullPage: true, animations: "disabled" });
    assert(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1), `${route} 不应撑宽手机页面`);
    assert(await page.locator(".card-head").evaluateAll(heads => heads.every(head => {
      const text = head.firstElementChild;
      return !head.querySelector(".card-actions") || text.getBoundingClientRect().width >= head.getBoundingClientRect().width - 64;
    })), `${route} 的说明文字不能被操作按钮挤成窄列`);
  }
  assert.deepEqual(errors, [], "页面不得出现 JavaScript 错误");
  console.log(`通过：首次设置、真实模型调用、七页桌面/手机、深色主题、调度跳转、嵌套弹窗、草稿保护；截图目录 ${directory}`);
} finally {
  await browser?.close();
  child.kill("SIGTERM");
  await new Promise(done => upstream.close(done));
}
