// 一键升级的真实浏览器验收（容器部署路径）。
//
// 容器路径现在与二进制路径共用同一段实现：下载 → 校验 sha256 → 替换自己 → 退出，
// 由 restart 策略拉起。这里把整条链路跑通，**包括真的替换掉一个可执行文件**：
//
//   1. 假 GitHub 提供 v9.9.9 的 tar.gz 与 checksums.txt（内容是真实 tar.gz，
//      sha256 也是真的）；
//   2. 服务端以 AKHUB_DEPLOY=docker + AKHUB_ALLOW_RESTART=1 启动，入口脚本的契约
//      由此满足；被测的"当前二进制"指向一个临时目录里的副本；
//   3. 点面板按钮 → 断言那个副本真的被换成了新内容，且旧版本被备份。
//
// 之所以能在这台 macOS 上验 Linux 的链路：AKHUB_ASSET_SUFFIX 覆盖平台标识，
// 假 Release 就发"Linux"资产。
import assert from "node:assert/strict";
import { mkdtemp, writeFile, chmod, readFile, copyFile, stat } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createServer } from "node:http";
import { spawn, execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { chromium } from "playwright-core";

const directory = await mkdtemp(join(tmpdir(), "akhub-oneclick-"));

// ---- 造一个真实的 tar.gz，内含 <stem>/akhub ----
const stage = join(directory, "stage", "akhub-v9.9.9-linux-x86_64");
await (await import("node:fs/promises")).mkdir(stage, { recursive: true });
const NEW_BINARY = "#!/bin/sh\necho \"akhub 9.9.9 (upgraded)\"\n";
await writeFile(join(stage, "akhub"), NEW_BINARY);
await chmod(join(stage, "akhub"), 0o755);
const archive = join(directory, "akhub-v9.9.9-linux-x86_64.tar.gz");
execFileSync("tar", ["-czf", archive, "-C", join(directory, "stage"), "akhub-v9.9.9-linux-x86_64"]);
const archiveBytes = await readFile(archive);
const digest = createHash("sha256").update(archiveBytes).digest("hex");

// ---- 假 GitHub：latest + 指定 tag 的 assets ----
const assetName = "akhub-v9.9.9-linux-x86_64.tar.gz";
const base = "http://127.0.0.1:";
const github = createServer((req, res) => {
  res.setHeader("content-type", "application/json");
  const url = req.url ?? "";
  if (url.startsWith("/dl/")) {
    res.setHeader("content-type", "application/gzip");
    res.end(archiveBytes);
    return;
  }
  if (url.startsWith("/checksums/")) {
    res.setHeader("content-type", "text/plain");
    res.end(digest + "  " + assetName + "\n");
    return;
  }
  const origin = "http://127.0.0.1:" + github.address().port;
  const release = {
    tag_name: "v9.9.9",
    name: "9.9.9",
    html_url: origin + "/releases/v9.9.9",
    published_at: "2026-10-16T00:00:00Z",
    body: "测试用 Release",
    assets: [
      { name: assetName, browser_download_url: origin + "/dl/" + assetName, size: archiveBytes.length },
      { name: "checksums.txt", browser_download_url: origin + "/checksums/checksums.txt", size: 100 },
    ],
  };
  if (url.startsWith("/releases/latest") || url.startsWith("/releases/tags/")) {
    res.end(JSON.stringify(release));
    return;
  }
  res.statusCode = 404;
  res.end("{}");
});
await new Promise((d) => github.listen(0, "127.0.0.1", d));
const githubBase = base + github.address().port;

const allocator = createServer();
await new Promise((d) => allocator.listen(0, "127.0.0.1", d));
const port = allocator.address().port;
await new Promise((d) => allocator.close(d));
const site = base + port;

// 被测的"正在运行的二进制"：拷一份到临时目录，升级应当替换它。
const installDir = join(directory, "install");
await (await import("node:fs/promises")).mkdir(installDir, { recursive: true });
const installed = join(installDir, "akhub");
await copyFile(resolve("../target/debug/akhub"), installed);
await chmod(installed, 0o755);
const before = await readFile(installed);

const child = spawn(installed, [], {
  env: {
    ...process.env,
    AKHUB_DATA_DIR: join(directory, "data"),
    AKHUB_LISTEN: "127.0.0.1:" + port,
    AKHUB_UPDATE_API: githubBase,
    AKHUB_DEPLOY: "docker",
    // 入口脚本在"二进制位于可写卷"时会设这个；容器路径的契约就是它。
    AKHUB_ALLOW_RESTART: "1",
    // 这台机器是 macOS，但被测的是 Linux 资产链路。
    AKHUB_ASSET_SUFFIX: "linux-x86_64",
    // 缩短关闭宽限：测试不必等默认的 180 秒。
    AKHUB_SHUTDOWN_GRACE_SECS: "2",
  },
  stdio: "ignore",
});

let browser;
try {
  for (let i = 0; i < 300; i++) {
    if (await fetch(site + "/health/ready").then((r) => r.ok, () => false)) break;
    await new Promise((d) => setTimeout(d, 100));
  }
  browser = await chromium.launch({ headless: true, executablePath: process.env.CHROME_PATH });
  const context = await browser.newContext({ viewport: { width: 1440, height: 1000 } });
  const page = await context.newPage();
  page.setDefaultTimeout(30000);
  const errors = [];
  page.on("pageerror", (e) => errors.push(e.message));
  await page.goto(site + "/admin");
  await page.getByLabel("密码", { exact: true }).fill("review-only-password-123");
  await page.getByRole("button", { name: "创建管理员", exact: true }).click();
  await page.locator(".nav-item").first().waitFor();

  const status = await (await context.request.get(site + "/admin/api/system/update?refresh=1")).json();
  console.log("deploy =", status.deploy, "| latest =", status.latest, "| can_one_click =", status.can_one_click);
  assert.equal(status.deploy, "docker", "本用例扮演容器部署");
  assert.equal(status.latest, "9.9.9");
  assert.equal(status.can_one_click, true, "容器里二进制可替换时必须能一键升级：" + JSON.stringify(status));

  await page.locator(".brand-version").click();
  const dialog = page.getByRole("dialog", { name: "版本与更新", exact: true });
  await dialog.waitFor();
  const button = dialog.getByRole("button", { name: /一键升级到 v9.9.9/ });
  await button.waitFor();
  console.log("一键升级按钮出现 =", await button.isVisible());
  await button.click();

  // 轮询进度。注意容器路径**会真的把进程关掉**（这正是它触发容器重启的方式），
  // 所以这里要能容忍"请求打到已经退出的进程"：拿到 done/failed、或者服务不再应答
  // 都算这一轮结束，然后靠磁盘上的结果与进程退出码下结论。
  let progress = { phase: "idle" };
  let exited = false;
  child.once("exit", () => {
    exited = true;
  });
  for (let i = 0; i < 300; i++) {
    if (exited) break;
    const s = await context.request
      .get(site + "/admin/api/system/update", { timeout: 2000 })
      .then((r) => (r.ok() ? r.json() : null))
      .catch(() => null);
    if (s) {
      progress = s.progress;
      if (progress.phase === "done" || progress.phase === "failed") break;
    }
    await new Promise((d) => setTimeout(d, 100));
  }
  console.log("最终进度 =", JSON.stringify(progress));
  assert.equal(progress.phase, "done", "一键升级应当成功：" + JSON.stringify(progress));
  assert.equal(progress.to, "9.9.9");

  // 关键行为：升级后进程要**自己退出**，好让容器 restart 策略把它拉起来。
  // 早期实现只替换文件、不退出，容器里就永远跑着旧进程。
  for (let i = 0; i < 100 && !exited; i++) {
    await new Promise((d) => setTimeout(d, 100));
  }
  assert(exited, "升级完成后进程必须自己退出，交给 restart 策略拉起");
  console.log("进程已退出（容器会被 restart 策略拉起）=", exited);

  // 关键断言：磁盘上那个可执行文件真的被换掉了。
  const after = await readFile(installed, "utf8");
  assert.equal(after, NEW_BINARY, "容器里的二进制必须被替换成新版本");
  assert.notDeepEqual(after, before, "内容必须与升级前不同");
  console.log("二进制已被替换 =", JSON.stringify(after.trim()));

  // 旧版本要留一份备份，便于回滚。
  const dir = await (await import("node:fs/promises")).readdir(installDir);
  const backups = dir.filter((n) => n.includes("bak-"));
  console.log("备份文件 =", JSON.stringify(dir));
  assert(backups.length >= 1, "升级前必须备份旧二进制：" + JSON.stringify(dir));
  assert.deepEqual(errors, [], "页面不得出现 JavaScript 错误");
  console.log("通过：容器部署一键升级——面板按钮 → 下载 → 校验 → 替换容器内二进制 → 备份旧版本");
} finally {
  await browser?.close();
  child.kill("SIGKILL");
  await new Promise((d) => github.close(d));
}