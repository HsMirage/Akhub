// 验收：升级完成后，页面必须等到**新版本**才刷新——不能在旧进程还在优雅关闭时
// 就刷新（那样用户看到的还是旧版本，以为升级没生效）。
//
// 做法：让"新进程"是一个报告 v9.9.9 的假二进制；旧进程是真实的 akhub（v1.1.31）。
// 监督者在旧进程退出后拉起假二进制（它只需应答 /health/version）。
import assert from "node:assert/strict";
import { mkdtemp, writeFile, chmod, mkdir, readFile, copyFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createServer } from "node:http";
import { spawn, execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { chromium } from "playwright-core";

const directory = await mkdtemp(join(tmpdir(), "akhub-vergate-"));

// 新二进制：一个极小的 HTTP 服务，只应答 /health/version 与 /health/ready。
const NEW = `#!/usr/bin/env node
const http = require("http");
const port = Number(process.env.AKHUB_LISTEN.split(":")[1]);
http.createServer((req, res) => {
  res.setHeader("content-type", "application/json");
  if (req.url.startsWith("/health/version")) {
    res.end(JSON.stringify({ object: "health", status: "ok", version: "9.9.9" }));
    return;
  }
  // 其它路径给一个带版本号的 HTML：页面刷新后读 title 就知道跑的是哪个版本。
  res.setHeader("content-type", "text/html");
  res.end("<!doctype html><title>v9.9.9</title>new process");
}).listen(port, "127.0.0.1");
`;
const stage = join(directory, "stage", "akhub-v9.9.9-linux-x86_64");
await mkdir(stage, { recursive: true });
await writeFile(join(stage, "akhub"), NEW);
await chmod(join(stage, "akhub"), 0o755);
const assetName = "akhub-v9.9.9-linux-x86_64.tar.gz";
const archive = join(directory, assetName);
execFileSync("tar", ["-czf", archive, "-C", join(directory, "stage"), "akhub-v9.9.9-linux-x86_64"]);
const bytes = await readFile(archive);
const digest = createHash("sha256").update(bytes).digest("hex");

const github = createServer((req, res) => {
  const url = req.url ?? "";
  const origin = "http://127.0.0.1:" + github.address().port;
  if (url.startsWith("/dl/")) { res.setHeader("content-type", "application/gzip"); res.end(bytes); return; }
  if (url.startsWith("/checksums/")) { res.setHeader("content-type", "text/plain"); res.end(digest + "  " + assetName + "\n"); return; }
  res.setHeader("content-type", "application/json");
  res.end(JSON.stringify({ tag_name: "v9.9.9", name: "9.9.9", html_url: origin + "/r", published_at: "2026-10-16T00:00:00Z", body: "x",
    assets: [
      { name: assetName, browser_download_url: origin + "/dl/" + assetName, size: bytes.length },
      { name: "checksums.txt", browser_download_url: origin + "/checksums/checksums.txt", size: 100 },
    ] }));
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

const env = {
  ...process.env,
  AKHUB_DATA_DIR: join(directory, "data"),
  AKHUB_LISTEN: "127.0.0.1:" + port,
  AKHUB_UPDATE_API: githubBase,
  AKHUB_DEPLOY: "docker",
  AKHUB_ALLOW_RESTART: "1",
  AKHUB_ASSET_SUFFIX: "linux-x86_64",
  // 故意留一段较长的优雅关闭：旧进程会在升级完成后继续应答一段时间，
  // 这正是要验证的窗口。
  AKHUB_SHUTDOWN_GRACE_SECS: "6",
};

let child = null;
let restarts = 0;
let supervising = true;
const supervise = () => {
  child = spawn(installed, [], { env, stdio: "ignore" });
  child.on("exit", () => {
    if (!supervising) return;
    restarts += 1;
    setTimeout(() => { if (supervising) supervise(); }, 200);
  });
};
supervise();

let browser;
try {
  for (let i = 0; i < 300; i++) {
    if (await fetch(base + "/health/ready").then((r) => r.ok, () => false)) break;
    await new Promise((d) => setTimeout(d, 100));
  }
  browser = await chromium.launch({ headless: true, executablePath: process.env.CHROME_PATH });
  const context = await browser.newContext({ viewport: { width: 1440, height: 1000 } });
  const page = await context.newPage();
  page.setDefaultTimeout(30000);

  // 记录页面每次加载时看到的版本（reload 会重新导航）。
  const navigations = [];
  page.on("framenavigated", (frame) => {
    if (frame === page.mainFrame()) navigations.push(frame.url());
  });

  await page.goto(base + "/admin");
  await page.getByLabel("密码", { exact: true }).fill("review-only-password-123");
  await page.getByRole("button", { name: "创建管理员", exact: true }).click();
  await page.locator(".nav-item").first().waitFor();
  navigations.length = 0;

  await page.locator(".brand-version").click();
  const dialog = page.getByRole("dialog", { name: "版本与更新", exact: true });
  await dialog.waitFor();
  await dialog.getByRole("button", { name: /一键升级到 v9.9.9/ }).click();

  // 等页面真的刷新到新版本（或超时）。
  let reloadedVersion = null;
  for (let i = 0; i < 160; i++) {
    const v = await fetch(base + "/health/version").then((r) => r.json()).then((j) => j.version).catch(() => null);
    if (v === "9.9.9") { reloadedVersion = v; break; }
    await new Promise((d) => setTimeout(d, 250));
  }
  console.log("新进程版本 =", reloadedVersion, "| 监督者重启次数 =", restarts);
  assert.equal(reloadedVersion, "9.9.9", "监督者应当拉起新版二进制");

  // 服务端已经换成新版，但**页面自己**是每 1 秒轮询一次才刷新的：
  // 这里要等浏览器跟上，否则断言的是"还没轮到它"而不是产品行为。
  //
  // 判据用 title：新进程返回的 HTML 里写着 v9.9.9。若页面在旧进程还应答时就
  // 刷新，它拿到的会是真 akhub 的 SPA（title 不含版本），断言就会失败。
  let title = null;
  for (let i = 0; i < 80; i++) {
    title = await page.title().catch(() => null);
    if (title === "v9.9.9") break;
    await new Promise((d) => setTimeout(d, 250));
  }
  console.log("页面导航次数 =", navigations.length, "| 最终 title =", JSON.stringify(title));
  assert.equal(
    title,
    "v9.9.9",
    "页面必须等到新进程上线才刷新（否则用户看到的还是旧版本）。导航记录：" +
      JSON.stringify(navigations),
  );
  console.log("通过：升级后页面等到新版本上线才刷新，不会在旧进程应答时提前刷新");
} finally {
  supervising = false;
  await browser?.close();
  child?.kill("SIGKILL");
  await new Promise((d) => github.close(d));
}