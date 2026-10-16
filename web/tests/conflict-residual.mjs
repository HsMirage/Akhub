// 残留场景：后台同步**真的**改了配置（上游新增了模型）时，
// 冲突必须被如实拦下，但用户输入的内容不能丢，且能原地恢复。
import assert from "node:assert/strict";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createServer } from "node:http";
import { spawn } from "node:child_process";
import { chromium } from "playwright-core";

const directory = await mkdtemp(join(tmpdir(), "akhub-residual-"));
let models = [{ id: "m1" }, { id: "m2" }];
const upstream = createServer((req, res) => {
  res.setHeader("content-type", "application/json");
  res.end(JSON.stringify(req.url === "/v1/models" ? { data: models } : { id: "x", choices: [], usage: {} }));
});
await new Promise((d) => upstream.listen(0, "127.0.0.1", d));
const allocator = createServer();
await new Promise((d) => allocator.listen(0, "127.0.0.1", d));
const port = allocator.address().port;
await new Promise((d) => allocator.close(d));
const base = "http://127.0.0.1:" + port;
const child = spawn(resolve("../target/debug/akhub"), [], {
  env: { ...process.env, AKHUB_DATA_DIR: join(directory, "data"), AKHUB_LISTEN: "127.0.0.1:" + port }, stdio: "ignore" });
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
  await page.goto(base + "/admin");
  await page.getByLabel("密码", { exact: true }).fill("review-only-password-123");
  await page.getByRole("button", { name: "创建管理员", exact: true }).click();
  await page.locator(".nav-item").first().waitFor();
  const post = async (p, data) => {
    const r = await context.request.post(base + "/admin/api" + p, { headers: { "x-akhub-csrf": "1" }, data });
    assert(r.ok(), p + ": " + r.status() + " " + (await r.text()));
    return r.json();
  };
  const g = await post("/groups", { name: "G", multiplier_limit: "2" });
  const a = await post("/accounts", { group_id: g.group.id, name: "A", base_url: "http://127.0.0.1:" + upstream.address().port,
    api_key: "k", preferred_protocol: "openai_chat", allow_private_network: true, multiplier_mode: "manual", manual_multiplier: "1" });
  await context.request.patch(base + "/admin/api/accounts/" + a.id, { headers: { "x-akhub-csrf": "1" }, data: { auto_sync: true } });
  await post("/accounts/" + a.id + "/models/sync");   // 收敛
  await page.goto(base + "/admin#/accounts");
  await page.reload();
  await page.locator("tbody tr[data-account-id]").first().waitFor();

  // 用户开始编辑。
  await page.locator("tbody tr[data-account-id]").first().locator("button").filter({ hasText: "编辑" }).first().click();
  const drawer = page.locator(".drawer").first();
  await drawer.waitFor();
  const nameField = drawer.getByLabel("名称", { exact: true });
  await nameField.fill("用户正在输入的名字");

  // 后台同步**真的**改了配置：上游新增了一个模型。
  models = [{ id: "m1" }, { id: "m2" }, { id: "m3-brand-new" }];
  await post("/accounts/" + a.id + "/models/sync");

  // 点保存：这次是真冲突，应当被拦下——但输入必须原地保留。
  await drawer.getByRole("button", { name: "保存", exact: true }).click();
  await page.waitForTimeout(1500);
  const dialog = page.getByRole("dialog", { name: "这份配置已经变了" });
  const shown = await dialog.count();
  console.log("真冲突时弹窗 =", shown);
  console.log("输入框内容仍在 =", JSON.stringify(await nameField.inputValue()));
  assert.equal(shown, 1, "真的改了配置时必须如实提示");
  assert.equal(await nameField.inputValue(), "用户正在输入的名字", "冲突不能抹掉用户输入");

  // 点「取回最新配置」：不重载页面，表单与输入原地保留。
  await dialog.getByRole("button", { name: "取回最新配置", exact: true }).click();
  await page.waitForTimeout(1500);
  console.log("取回后 URL 未变 =", page.url().includes("#/accounts"));
  console.log("取回后输入仍在 =", JSON.stringify(await nameField.inputValue()));

  // 再点一次保存：这次必须成功，且内容落库。
  await drawer.getByRole("button", { name: "保存", exact: true }).click();
  await page.waitForTimeout(2000);
  const listed = await context.request.get(base + "/admin/api/accounts");
  const saved = (await listed.json()).data.find((x) => x.id === a.id);
  console.log("最终落库 =", JSON.stringify(saved.name));
  assert.equal(saved.name, "用户正在输入的名字", "取回后重试保存必须成功");
  console.log("通过：真冲突被拦下 → 输入不丢 → 取回后原地重试成功");
} finally { await browser?.close(); child.kill("SIGTERM"); await new Promise((d) => upstream.close(d)); }
