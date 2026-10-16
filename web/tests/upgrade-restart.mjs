// 容器路径的**真实重启**验收：升级后进程自己退出，由 restart 策略拉起新版本。
//
// 复审发现两个只有"真换掉进程"才暴露的问题，这个用例把它们钉住：
//
//   1. 升级完成后进程必须真的退出——否则容器里永远跑着旧进程；
//   2. 进程被换掉之后，面板必须进入"等待新进程上线"，而不是永久卡在"升级中…"。
//      （早期实现只认服务端的 done，而容器路径下 done 往往来不及被前端看到。）
//
// 这里用"外层监督者"模拟 docker 的 restart 策略：子进程退出后自动拉起同一个可执行
// 文件——正是容器里发生的事。
import assert from "node:assert/strict";
import { mkdtemp, writeFile, chmod, mkdir, readFile, copyFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createServer } from "node:http";
import { spawn, execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { chromium } from "playwright-core";

const directory = await mkdtemp(join(tmpdir(), "akhub-restart-"));

const stage = join(directory, "stage", "akhub-v9.9.9-linux-x86_64");
await mkdir(stage, { recursive: true });
// 新版二进制是个能应答 --version 的脚本，好让监督者"拉起"它。
await writeFile(join(stage, "akhub"), "#!/bin/sh\necho \"akhub 9.9.9 (upgraded)\"\n");
await chmod(join(stage, "akhub"), 0o755);
const assetName = "akhub-v9.9.9-linux-x86_64.tar.gz";
const archive = join(directory, assetName);
execFileSync("tar", ["-czf", archive, "-C", join(directory, "stage"), "akhub-v9.9.9-linux-x86_64"]);
const bytes = await readFile(archive);
const digest = createHash("sha256").update(bytes).digest("hex");

const github = createServer((req, res) => {
  const url = req.url ?? "";
  const origin = "http://127.0.0.1:" + github.address().port;
  if (url.startsWith("/dl/")) {
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

const env = {
  ...process.env,
  AKHUB_DATA_DIR: join(directory, "data"),
  AKHUB_LISTEN: "127.0.0.1:" + port,
  AKHUB_UPDATE_API: githubBase,
  AKHUB_DEPLOY: "docker",
  AKHUB_ALLOW_RESTART: "1",
  AKHUB_ASSET_SUFFIX: "linux-x86_64",
  AKHUB_SHUTDOWN_GRACE_SECS: "2",
};

// 监督者：模拟容器的 restart 策略。子进程退出就重新拉起同一个文件。
let child = null;
let restarts = 0;
let supervising = true;
const supervise = () => {
  child = spawn(installed, [], { env, stdio: "ignore" });
  child.on("exit", () => {
    if (!supervising) return;
    restarts += 1;
    // 模拟容器的 restart 策略：稍等片刻再拉起同一个可执行文件。
    setTimeout(() => {
      if (supervising) supervise();
    }, 300);
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
  await page.goto(base + "/admin");
  await page.getByLabel("密码", { exact: true }).fill("review-only-password-123");
  await page.getByRole("button", { name: "创建管理员", exact: true }).click();
  await page.locator(".nav-item").first().waitFor();

  await page.locator(".brand-version").click();
  const dialog = page.getByRole("dialog", { name: "版本与更新", exact: true });
  await dialog.waitFor();
  await dialog.getByRole("button", { name: /一键升级到 v9.9.9/ }).click();

  // 面板必须最终离开"正在升级"，进入"等待新进程上线"（或已经刷新到新版本）。
  let sawWaiting = false;
  let text = "";
  for (let i = 0; i < 160; i++) {
    text = await dialog.innerText().catch(() => "");
    if (text.includes("等待新进程上线")) { sawWaiting = true; break; }
    await new Promise((d) => setTimeout(d, 250));
  }
  console.log("升级中面板 =", JSON.stringify(text.replace(/\n+/g, " | ").slice(0, 120)));
  console.log("面板进入等待 =", sawWaiting, "| 监督者重启次数 =", restarts);
  assert(restarts >= 1, "升级后进程必须退出，好让 restart 策略拉起新版本");
  assert(sawWaiting, "进程被换掉后，面板必须进入等待新进程，而不是卡在升级中");

  // 关键断言：磁盘上的二进制真的换成了新版本。
  const after = await readFile(installed, "utf8");
  assert(after.includes("9.9.9"), "容器里的二进制必须被替换：" + JSON.stringify(after.slice(0, 80)));
  console.log("二进制已替换 =", JSON.stringify(after.trim()));
  console.log("通过：容器路径升级后进程退出 → restart 策略拉起 → 面板进入等待新进程");
} finally {
  supervising = false;
  await browser?.close();
  child.kill("SIGKILL");
  await new Promise((d) => github.close(d));
}