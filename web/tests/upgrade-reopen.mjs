// 升级期间重开面板：必须继续跟踪进度，而不是卡在"正在升级…"。
//
// 复审发现：早期实现只按组件本地的 `upgrading` 决定要不要轮询。关掉面板会卸载
// 组件，重开时它是 false，而服务端还在跑——面板于是显示着服务端的 running，
// 却再也不轮询，永远等不到结束。
//
// 这里让"升级"慢下来（假 GitHub 的下载故意延迟），好让测试有机会关掉再打开。
import assert from "node:assert/strict";
import { mkdtemp, writeFile, chmod, mkdir, readFile, copyFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createServer } from "node:http";
import { spawn, execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { chromium } from "playwright-core";

const directory = await mkdtemp(join(tmpdir(), "akhub-reopen-"));

const stage = join(directory, "stage", "akhub-v9.9.9-linux-x86_64");
await mkdir(stage, { recursive: true });
await writeFile(join(stage, "akhub"), "#!/bin/sh\necho \"akhub 9.9.9\"\n");
await chmod(join(stage, "akhub"), 0o755);
const assetName = "akhub-v9.9.9-linux-x86_64.tar.gz";
const archive = join(directory, assetName);
execFileSync("tar", ["-czf", archive, "-C", join(directory, "stage"), "akhub-v9.9.9-linux-x86_64"]);
const bytes = await readFile(archive);
const digest = createHash("sha256").update(bytes).digest("hex");

const github = createServer(async (req, res) => {
  const url = req.url ?? "";
  const origin = "http://127.0.0.1:" + github.address().port;
  if (url.startsWith("/dl/")) {
    // 下载故意慢：给测试留出"关掉面板再打开"的时间窗。
    await new Promise((d) => setTimeout(d, 2500));
    res.setHeader("content-type", "application/gzip");
    res.end(bytes);
    return;
  }
  if (url.startsWith("/checksums/")) {
    res.setHeader("content-type", "text/plain");
    res.end(digest + "  " + assetName + "\n");
    return;
  }
  res.setHeader("content-type", "application/json");
  res.end(JSON.stringify({
    tag_name: "v9.9.9", name: "9.9.9", html_url: origin + "/r",
    published_at: "2026-10-16T00:00:00Z", body: "x",
    assets: [
      { name: assetName, browser_download_url: origin + "/dl/" + assetName, size: bytes.length },
      { name: "checksums.txt", browser_download_url: origin + "/checksums/checksums.txt", size: 100 },
    ],
  }));
});
await new Promise((d) => github.listen(0, "127.0.0.1", d));
const githubBase = "http://127.0.0.1:" + github.address().port;

const allocator = createServer();
await new Promise((d) => allocator.listen(0, "127.0.0.1", d));
const port = allocator.address().port;
await new Promise((d) => allocator.close(d));
const base = "http://127.0.0.1:" + port;

const installDir = join(directory, "install");
await mkdir(installDir, { recursive: true });
const installed = join(installDir, "akhub");
await copyFile(resolve("../target/debug/akhub"), installed);
await chmod(installed, 0o755);

const child = spawn(installed, [], {
  env: {
    ...process.env,
    AKHUB_DATA_DIR: join(directory, "data"),
    AKHUB_LISTEN: "127.0.0.1:" + port,
    AKHUB_UPDATE_API: githubBase,
    AKHUB_DEPLOY: "docker",
    AKHUB_ALLOW_RESTART: "1",
    AKHUB_ASSET_SUFFIX: "linux-x86_64",
    // 缩短关闭宽限：测试不必等 180 秒。
    AKHUB_SHUTDOWN_GRACE_SECS: "2",
  },
  stdio: "ignore",
});

let browser;
try {
  for (let i = 0; i < 300; i++) {
    if (await fetch(base + "/health/ready").then((r) => r.ok, () => false)) break;
    await new Promise((d) => setTimeout(d, 100));
  }
  browser = await chromium.launch({ headless: true, executablePath: process.env.CHROME_PATH });
  const context = await browser.newContext({ viewport: { width: 1440, height: 1000 } });
  const page = await context.newPage();
  page.setDefaultTimeout(20000);
  await page.goto(base + "/admin");
  await page.getByLabel("密码", { exact: true }).fill("review-only-password-123");
  await page.getByRole("button", { name: "创建管理员", exact: true }).click();
  await page.locator(".nav-item").first().waitFor();

  await page.locator(".brand-version").click();
  const dialog = page.getByRole("dialog", { name: "版本与更新", exact: true });
  await dialog.waitFor();
  await dialog.getByRole("button", { name: /一键升级到 v9.9.9/ }).click();

  // 等服务端进入 running（下载要 2.5 秒，来得及）。
  let running = false;
  for (let i = 0; i < 80; i++) {
    const s = await context.request.get(base + "/admin/api/system/update").then((r) => (r.ok() ? r.json() : null)).catch(() => null);
    if (s && s.progress.phase === "running") { running = true; break; }
    await new Promise((d) => setTimeout(d, 50));
  }
  assert(running, "升级应当进入 running");

  // 关掉面板，再重新打开。
  await page.keyboard.press("Escape");
  await dialog.waitFor({ state: "hidden" });
  await page.locator(".brand-version").click();
  await dialog.waitFor();
  const reopened = await dialog.innerText();
  console.log("重开后的面板 =", JSON.stringify(reopened.replace(/\n+/g, " | ").slice(0, 140)));
  assert(
    reopened.includes("正在升级到") || reopened.includes("等待新进程"),
    "重开面板后应当仍在跟踪这次升级：" + reopened.replace(/\n+/g, " | ").slice(0, 200),
  );

  // 关键：重开之后必须继续跟踪，最终离开"正在升级"状态。
  let settled = false;
  for (let i = 0; i < 120; i++) {
    const text = await dialog.innerText().catch(() => "");
    if (!text.includes("正在升级到")) { settled = true; break; }
    await new Promise((d) => setTimeout(d, 250));
  }
  console.log("重开后仍能跟踪到结束 =", settled);
  assert(settled, "重开面板后必须继续跟踪进度，不能卡在「正在升级…」");
  console.log("通过：升级期间重开面板仍会继续跟踪进度");
} finally {
  await browser?.close();
  child.kill("SIGKILL");
  await new Promise((d) => github.close(d));
}