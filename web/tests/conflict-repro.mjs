// 复现用户场景：填表单 → 后台托管同步推进配置 → 点保存。
// 断言：不再出现"配置已被其他会话修改"，且输入的内容不丢、保存真的落库。
import assert from "node:assert/strict";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createServer } from "node:http";
import { spawn } from "node:child_process";
import { chromium } from "playwright-core";

const directory = await mkdtemp(join(tmpdir(), "akhub-conflict-"));
const upstream = createServer((req, res) => {
  res.setHeader("content-type", "application/json");
  res.end(JSON.stringify(req.url === "/v1/models"
    ? { data: [{ id: "m1" }, { id: "m2" }] }
    : { id: "x", choices: [{ message: { role: "assistant", content: "ok" }, finish_reason: "stop" }], usage: { prompt_tokens: 1, completion_tokens: 1 } }));
});
await new Promise((d) => upstream.listen(0, "127.0.0.1", d));
const allocator = createServer();
await new Promise((d) => allocator.listen(0, "127.0.0.1", d));
const port = allocator.address().port;
await new Promise((d) => allocator.close(d));
const base = "http://127.0.0.1:" + port;
const child = spawn(resolve("../target/debug/akhub"), [], {
  env: { ...process.env, AKHUB_DATA_DIR: join(directory, "data"), AKHUB_LISTEN: "127.0.0.1:" + port },
  stdio: "ignore",
});
let browser;
try {
  for (let i = 0; i < 100; i++) {
    if (child.exitCode !== null) throw new Error("测试服务提前退出");
    if (await fetch(base + "/health/ready").then((r) => r.ok, () => false)) break;
    await new Promise((d) => setTimeout(d, 100));
  }
  browser = await chromium.launch({ headless: true, executablePath: process.env.CHROME_PATH });
  const context = await browser.newContext({ viewport: { width: 1440, height: 1000 } });
  const page = await context.newPage();
  page.setDefaultTimeout(10000);
  const errors = [];
  page.on("pageerror", (e) => errors.push(e.message));
  await page.goto(base + "/admin");
  await page.getByLabel("密码", { exact: true }).fill("review-only-password-123");
  await page.getByRole("button", { name: "创建管理员", exact: true }).click();
  await page.locator(".nav-item").first().waitFor();

  const post = async (path, data) => {
    const response = await context.request.post(base + "/admin/api" + path, { headers: { "x-akhub-csrf": "1" }, data });
    assert(response.ok(), path + ": " + response.status() + " " + (await response.text()));
    return response.json();
  };
  const group = await post("/groups", { name: "验收分组", multiplier_limit: "2" });
  const account = await post("/accounts", {
    group_id: group.group.id, name: "验收账号", base_url: "http://127.0.0.1:" + upstream.address().port,
    api_key: "review-fake-key", preferred_protocol: "openai_chat", allow_private_network: true,
    multiplier_mode: "manual", manual_multiplier: "1",
  });
  // 开托管，让后台同步成为"会重建配置快照"的那条路径（正是误报的来源）。
  await context.request.patch(base + "/admin/api/accounts/" + account.id, {
    headers: { "x-akhub-csrf": "1" }, data: { auto_sync: true },
  });
  // 先跑一轮同步让目录与目标收敛。这一轮**确实**会改配置（从无到有建出
  // 目标），版本前进是应该的；现场症状发生在收敛之后的稳态里。
  await post("/accounts/" + account.id + "/models/sync");

  // 哈希路由不触发重新加载，必须显式 reload 才能拿到刚建的数据。
  await page.goto(base + "/admin#/accounts");
  await page.reload();
  await page.locator("tbody tr[data-account-id]").first().waitFor();

  // 1) 打开编辑抽屉，填一段内容——这就是用户"刚把信息输入完"的那一刻。
  const row = page.locator("tbody tr[data-account-id]").first();
  await row.locator("button").filter({ hasText: "编辑" }).first().click();
  const drawer = page.locator(".drawer").first();
  await drawer.waitFor();
  const nameField = drawer.getByLabel("名称", { exact: true });
  await nameField.fill("改过名字的账号");
  assert.equal(await nameField.inputValue(), "改过名字的账号");

  // 2) 后台跑一轮托管同步（等价于 30 分钟一轮的 tasks::model_sync）。
  await post("/accounts/" + account.id + "/models/sync");

  // 3) 点保存：必须成功，且不能弹"这份配置已经变了"。
  await drawer.getByRole("button", { name: "保存", exact: true }).click();
  await page.waitForTimeout(2000);
  const conflictCount = await page.getByRole("dialog", { name: "这份配置已经变了" }).count();
  const bodyText = await page.locator("body").innerText();
  console.log("冲突弹窗数量 =", conflictCount);
  console.log("出现旧文案 =", bodyText.includes("配置已被其他会话修改"));
  console.log("出现新文案 =", bodyText.includes("这份配置已经变了"));

  // 4) 保存必须真的落库。
  const listed = await context.request.get(base + "/admin/api/accounts");
  const saved = (await listed.json()).data.find((a) => a.id === account.id);
  console.log("落库的名字 =", JSON.stringify(saved.name));

  assert.equal(conflictCount, 0, "后台同步不该弹出配置冲突对话框");
  assert.equal(bodyText.includes("配置已被其他会话修改"), false, "不该出现旧文案");
  assert.equal(saved.name, "改过名字的账号", "保存的内容必须落库");
  assert.deepEqual(errors, [], "页面不得出现 JavaScript 错误");
  console.log("通过：填表单 → 后台同步 → 保存成功，无冲突误报、无输入丢失");
} finally {
  await browser?.close();
  child.kill("SIGTERM");
  await new Promise((d) => upstream.close(d));
}
