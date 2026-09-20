# Windows 部署

Akhub 的 Windows 原生部署（§25.2）：**一个 `akhub.exe` 加一个数据目录**。发布矩阵里 Windows 只有 `windows-x86_64` 一个目标，Windows 10 / 11 与 Windows Server 2016 及以上均可直接运行，不需要 .NET、Node、Java 或任何容器运行时。

官方没有把 Windows 做成服务安装包，所以本机跑起来只有两步：把 exe 放到 `C:\Program Files\Akhub` 目录下，把数据目录固定在 `C:\ProgramData\Akhub`，然后选一个服务托管方式（§4）。**最容易踩坑的一处是停止超时**：Akhub 收到停止信号后最多要给在途的长流式请求 180 秒（§25.3），Windows 侧不把停止超时调到 180 秒以上，就会把正在进行中的流式响应强杀掉。

> 下文命令是 Windows PowerShell 5.1 与 PowerShell 7 通用语法；涉及注册服务、改 ACL、放行防火墙的部分必须在**管理员**窗口里执行。

---

## 1. 下载与校验

每个 Release 提供**两种** Windows 资产，按需要挑一个：

| 资产 | 大小 | 适合 |
|---|---|---|
| `akhub-<Tag>-windows-x86_64.exe` | 约 18 MB | **只想跑起来**：下载双击即可，不用解压 |
| `akhub-<Tag>-windows-x86_64.zip` | 约 7 MB | **要文档和一键脚本**：内含本文、`install.ps1`、`deploy\` 示例 |

两者里的 `akhub.exe` **完全同一个文件**，只是 zip 压过、另外捎上了文档，所以体积只有三分之一。
功能没有任何差别，随便挑；想省流量就下 zip，想省事就下 exe。

下面按 zip 写（本文的路由、注册服务等章节都在包里）。只下了 exe 的话，
把「解压」那一步跳过即可，其余命令把路径换成 exe 所在位置。

Release 资产名与 `checksums.txt` 里的文件名完全一致，文件名里的版本号是 **Release 标签（带 `v`）**，例如 `akhub-v1.1.2-windows-x86_64.zip`。

```
$Tag   = 'v1.1.2'                 # Release 标签，带 v
$Repo  = 'HsMirage/Akhub'
$Asset = "akhub-$Tag-windows-x86_64.zip"
$Base  = "https://github.com/$Repo/releases/download/$Tag"
$Work  = "$env:USERPROFILE\Downloads\akhub-$Tag"

New-Item -ItemType Directory -Force -Path $Work | Out-Null
Set-Location $Work

Invoke-WebRequest -UseBasicParsing -Uri "$Base/$Asset"        -OutFile $Asset
Invoke-WebRequest -UseBasicParsing -Uri "$Base/checksums.txt" -OutFile checksums.txt
```

> 只想要裸 exe 的话，把上面两行的 `$Asset` 换成
> `"akhub-$Tag-windows-x86_64.exe"`，并跳过下面的 `Expand-Archive`。

对照 `checksums.txt` 校验 SHA256（`sha256sum` 格式是 `<hash>` 加两个空格再加 `./<文件名>`，所以取每行的第一段）：

```
$line = Select-String -Path .\checksums.txt -Pattern 'windows-x86_64\.zip$' | Select-Object -First 1
if (-not $line) { throw 'checksums.txt 里没有 windows-x86_64.zip' }

$expected = ($line.Line -split '\s+')[0].ToUpper()
$actual   = (Get-FileHash ".\$Asset" -Algorithm SHA256).Hash
"期望 $expected"
"实际 $actual"
if ($actual -ne $expected) { throw 'SHA256 不匹配：不要解压，删掉重下' }
```

校验通过后再解压：

```
Expand-Archive -Path ".\$Asset" -DestinationPath . -Force
Get-ChildItem ".\akhub-$Tag-windows-x86_64" | Select-Object Name, Length
```

压缩包根目录是 `akhub-<Tag>-windows-x86_64\`，里面有 `akhub.exe`、`README.md`、`NOTICES.md`、`LICENSE` 和 `deploy\`（本文也在里面）。

仓库根目录还带一个官方的一键安装/升级脚本 `install.ps1`，它把上面下载、校验、解压、安装、备份旧版本这条链路做完了，适合做批量部署：

```
# 先下看一眼再跑（推荐）
Invoke-WebRequest -UseBasicParsing -Uri 'https://raw.githubusercontent.com/HsMirage/Akhub/master/install.ps1' -OutFile install.ps1
.\install.ps1 -Version v1.1.2 -AddToPath

# 指定版本、自定义安装目录、只演练不落地
.\install.ps1 -Version v1.1.2 -Dir 'C:\Program Files\Akhub'   # 指定安装目录
.\install.ps1 -Version v1.1.2 -DryRun
```

脚本参数：`-Version`（默认取最新 Release）、`-Dir`（默认 `%LOCALAPPDATA%\Programs\Akhub`）、`-Repo`、`-NoVerify`（跳过 sha256，不推荐）、`-AddToPath`、`-DryRun`。它默认装到用户目录，本手册下面统一按 `C:\Program Files\Akhub` 写，两者取其一即可；用脚本装完照样可以继续按本文配服务。

PowerShell 默认禁止运行未签名脚本时，只放开当前会话（不改系统设置）：

```
Set-ExecutionPolicy -Scope Process -ExecutionPolicy Bypass
```

---

## 2. 解压与目录规划

固定的两个位置，后面所有命令都按这两个路径写：

| 用途 | 路径 | 说明 |
| --- | --- | --- |
| 程序 | `C:\Program Files\Akhub\akhub.exe` | 只有管理员可写，防篡改 |
| 数据 | `C:\ProgramData\Akhub` | SQLite、`master.key`、临时文件 |
| 日志 | `C:\ProgramData\Akhub\logs` | 服务托管输出，目录必须先建出来 |

```
New-Item -ItemType Directory -Force -Path 'C:\Program Files\Akhub', 'C:\ProgramData\Akhub\logs' | Out-Null
Copy-Item ".\akhub-$Tag-windows-x86_64\akhub.exe" 'C:\Program Files\Akhub\akhub.exe' -Force
& 'C:\Program Files\Akhub\akhub.exe' --version      # 输出形如 akhub 1.1.0，应与标签一致
```

两点必须说清楚：

- **数据目录绝对不能省。** 服务进程的工作目录默认是 `C:\Windows\System32`，不显式设 `AKHUB_DATA_DIR` 时默认值 `./data` 会落到那儿去，权限和清理策略都不对。
- **不要放网络盘。** SQLite 走 SMB 会带来锁语义问题，数据目录必须是本机 NTFS 卷。

---

## 3. 直接前台运行（先验证能跑起来）

先用普通用户前台跑一次，确认能监听、能建库、能打开后台，再谈注册服务。

```
$env:AKHUB_DATA_DIR = 'C:\ProgramData\Akhub'
$env:AKHUB_LISTEN   = '127.0.0.1:8080'
& 'C:\Program Files\Akhub\akhub.exe'
```

启动成功时标准错误里会出现（Akhub 的日志全部走 stderr）：

```
INFO 数据目录已就绪 data_dir=C:\ProgramData\Akhub
INFO Akhub 已启动，管理后台位于 /admin addr=127.0.0.1:8080
```

浏览器打开 `http://127.0.0.1:8080/admin` 。首次访问是**首次设置页**：需要一个用户名和至少 8 个字符的口令，接口 `POST /admin/api/setup` 只能成功一次。同时检查这些文件已经生成：

```
Get-ChildItem 'C:\ProgramData\Akhub' -Force | Select-Object Name, Length
```

- `master.key`：32 字节主密钥。**丢了就无法恢复数据库里加密保存的上游 API Key**，没有第二把。
- `akhub.sqlite`（以及运行时出现的 `-wal` / `-shm`）：配置、账号、请求记录。

停止：在窗口里按 `Ctrl+C`。这就是优雅关闭入口——停止接新请求 → 取消排队请求 → 给在途请求最多 `AKHUB_SHUTDOWN_GRACE_SECS`（默认 180）秒 → 刷盘退出。窗口右上角的关闭按钮同样会触发控制台关闭事件。

---

## 4. 注册为 Windows 服务

### 4.1 两条路径共同的：服务账户与数据目录 ACL

**账户怎么选**

- **LocalSystem（默认，推荐先用它跑通）**：不需要口令、不会过期；Akhub 只需要读写自己的数据目录和监听环回端口，LocalSystem 完全够用，这也是 NSSM 与 WinSW 的默认值。
- **专用账户（要最小权限时）**：让 Akhub 以专门的本地用户或 `NT AUTHORITY\LocalService` 运行。换账户必须同时做两件事——给数据目录授权，以及确认新账户**能读到已有的 `master.key`**：读不到时 Akhub 会在启动阶段直接报错退出（不会悄悄换一把新密钥）；但如果文件是被删掉而不是读不到，进程会生成全新的一把，此时库里已加密的上游 Key 全部无法解密。

**数据目录 ACL（`icacls`）**

先建目录，再关继承、只留 SYSTEM 与本机管理员，然后把这个权限传播到已有子项：

```
New-Item -ItemType Directory -Force -Path 'C:\ProgramData\Akhub', 'C:\ProgramData\Akhub\logs' | Out-Null

icacls 'C:\ProgramData\Akhub' /inheritance:r /grant:r '*S-1-5-18:(OI)(CI)F' /grant:r '*S-1-5-32-544:(OI)(CI)F' /T
icacls 'C:\ProgramData\Akhub'      # 复核结果
```

- `*S-1-5-18` = `NT AUTHORITY\SYSTEM`，`*S-1-5-32-544` = 本机 `BUILTIN\Administrators`；用 SID 是为了不受系统显示语言影响。
- `(OI)(CI)F`：对象继承 + 容器继承 + 完全控制；`/T` 让已有的 `logs` 子目录一起拿到同样的 ACE。

如果改用专用账户，给它 **M（修改）** 就够，不要给 F：

```
# 本地用户 .\akhub 为例
icacls 'C:\ProgramData\Akhub' /grant:r "$env:COMPUTERNAME\akhub:(OI)(CI)M" /T

# 用内置服务账户 LocalService 为例
icacls 'C:\ProgramData\Akhub' /grant:r 'NT AUTHORITY\LocalService:(OI)(CI)M' /T
```

### 4.2 (A) NSSM —— 推荐，最省事

NSSM 是免安装的单文件服务包装器（public domain），它替你把控制台程序托管成服务，并且**原生支持把 Control-C 送给子进程**，正好对上 Akhub 的优雅关闭入口。

**下载。** 用 nssm.cc 的 CI 预发布版 `2.24-101`：稳定版 2.24（2014）在 Windows 10 Creators Update 之后有“服务起不来”的已知问题，nssm.cc 下载页自己就建议用预发布版；winget 里的 `NSSM.NSSM` 也是这个版本。

```
$NssmZip = "$env:TEMP\nssm.zip"
Invoke-WebRequest -UseBasicParsing -Uri 'https://nssm.cc/ci/nssm-2.24-101-g897c7ad.zip' -OutFile $NssmZip
Expand-Archive -Path $NssmZip -DestinationPath "$env:TEMP\nssm" -Force
Copy-Item "$env:TEMP\nssm\nssm-2.24-101-g897c7ad\win64\nssm.exe" 'C:\Program Files\Akhub\nssm.exe' -Force

# 可选：命令行里直接敲 nssm（追加到系统 PATH）
[Environment]::SetEnvironmentVariable('Path', [Environment]::GetEnvironmentVariable('Path','Machine') + ';C:\Program Files\Akhub', 'Machine')
```

**安装并配置。** `nssm install <服务名>` 不带程序时会弹 GUI 窗口，脚本里一定带程序路径；其余参数用 `nssm set` 逐条写死。

```
$Nssm = 'C:\Program Files\Akhub\nssm.exe'
$Bin  = 'C:\Program Files\Akhub\akhub.exe'

# 1) 注册（带程序路径 = 不弹 GUI）
& $Nssm install Akhub $Bin

# 2) Application / AppParameters / AppDirectory
& $Nssm set Akhub Application   $Bin
& $Nssm reset Akhub AppParameters                  # 无参数即启动服务
& $Nssm set Akhub AppDirectory  'C:\Program Files\Akhub'

# 3) 服务元信息与启动类型
& $Nssm set Akhub DisplayName 'Akhub AI 协议网关'
& $Nssm set Akhub Description 'AI 协议网关与组内负载均衡，管理后台 /admin'
& $Nssm set Akhub Start       SERVICE_AUTO_START

# 4) 环境变量（一条命令可给多个 KEY=VALUE，NSSM 会写成多字符串）
& $Nssm set Akhub AppEnvironmentExtra `
    'AKHUB_DATA_DIR=C:\ProgramData\Akhub' `
    'AKHUB_LISTEN=127.0.0.1:8080' `
    'AKHUB_SHUTDOWN_GRACE_SECS=180' `
    'RUST_LOG=akhub=info,warn'

# 5) 日志（目录必须先存在，否则 NSSM 会启动失败）
& $Nssm set Akhub AppStdout 'C:\ProgramData\Akhub\logs\akhub.out.log'
& $Nssm set Akhub AppStderr 'C:\ProgramData\Akhub\logs\akhub.err.log'
& $Nssm set Akhub AppRotateFiles  1
& $Nssm set Akhub AppRotateOnline 1
& $Nssm set Akhub AppRotateBytes  10485760        # 10 MiB 触发滚动

# 6) 关键的停止超时：让 Control-C 之后最多等 240 秒
& $Nssm set Akhub AppStopMethodConsole 240000

# 7) 退出即重启
& $Nssm set Akhub AppExit Default Restart
& $Nssm set Akhub AppRestartDelay 2000
```

**`AppStopMethodConsole` 为什么必须是 240000。** 停止服务时 NSSM 依次尝试：给控制台发 Control-C（等 `AppStopMethodConsole` 毫秒）→ 发 WM_CLOSE（等 `AppStopMethodWindow`）→ 发 WM_QUIT（等 `AppStopMethodThreads`）→ `TerminateProcess` 强杀。**这三个等待的默认值都是 1500 毫秒**，也就是说默认配置下 NSSM 会在 1.5 秒后直接强杀——对 Akhub 意味着正在进行的流式响应被硬切断，正好踩中 §25.3 要避免的事。把 Console 这一项设成 240000（240 秒）之后：Akhub 会收到 Control-C 并进入优雅关闭，最多用 180 秒收尾，通常在远小于 240 秒时自己退出，后面的 WM_CLOSE 与强杀都不会走到。只有 Console 这一项需要改。

240 的来历：`AKHUB_SHUTDOWN_GRACE_SECS`（默认 180）+ 刷盘与日志收尾的余量，与 Linux 侧 `TimeoutStopSec=200`、容器侧 `stop_grace_period: 200s` 的取法一致。

**启动与验证**（`nssm start` 不等待，加一点延时再查状态）：

```
& $Nssm start Akhub
Start-Sleep -Seconds 2
Get-Service Akhub | Select-Object Name, Status, StartType
(Get-CimInstance Win32_Service -Filter "Name='Akhub'").StartName    # 当前服务账户
```

复核参数是否真的写进去了（读注册表最直接）：

```
Get-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Services\Akhub\Parameters' |
    Select-Object Application, AppDirectory, AppStopMethodConsole, AppStdout, AppStderr
& $Nssm get Akhub AppEnvironmentExtra     # 逐行打印已配置的环境变量
```

**停止 / 卸载：**

```
& $Nssm stop Akhub
& $Nssm remove Akhub confirm     # confirm 才会跳过确认框
```

想改用专用账户：

```
& $Nssm set Akhub ObjectName 'NT AUTHORITY\LocalService'
# 或本地用户（口令是第二个值）：
& $Nssm set Akhub ObjectName '.\akhub' 'YourPasswordHere'
# 然后按 §4.1 给该账户授数据目录权限，再停止/启动一次
```

### 4.3 (B) WinSW —— 不依赖任何第三方 GUI 的原生包装器

**`sc.exe` 单独用不了，这点必须说清楚：** `sc.exe create` 只能登记一个可执行文件路径，它要求目标程序自己实现 Windows 服务控制协议（调用 `StartServiceCtrlDispatcher` 并响应 SCM 的控制请求）。`akhub.exe` 是普通控制台程序，不实现这套协议，直接 `sc.exe create Akhub binPath= "C:\Program Files\Akhub\akhub.exe"` 的结果是服务启动时报 **1053「服务没有及时响应启动或控制请求」**，永远起不来。`sc.exe` 的正确用途是管理已经注册好的服务（`sc.exe start` / `stop` / `delete` / `query`），而不是托管控制台程序。要一个不用 GUI 的原生做法，用 **WinSW**：它就是那个“服务外壳”。

WinSW 是单文件包装器，不依赖任何运行时（x64 版是自包含构建），配置全部落在一个 XML 里，天然适合脚本化和放进版本库。

```
Invoke-WebRequest -UseBasicParsing `
    -Uri 'https://github.com/winsw/winsw/releases/download/v2.12.0/WinSW-x64.exe' `
    -OutFile 'C:\Program Files\Akhub\akhub-service.exe'
```

WinSW 用“自己的文件名”去找同目录同名的 XML，所以必须是 `akhub-service.exe` 搭配 `akhub-service.xml`。

新建 `C:\Program Files\Akhub\akhub-service.xml`（UTF-8 编码）：

```
<service>
  <id>Akhub</id>
  <name>Akhub AI 协议网关</name>
  <description>AI 协议网关与组内负载均衡，管理后台 /admin</description>

  <executable>C:\Program Files\Akhub\akhub.exe</executable>
  <workingdirectory>C:\Program Files\Akhub</workingdirectory>
  <startmode>Automatic</startmode>

  <env name="AKHUB_DATA_DIR"            value="C:\ProgramData\Akhub" />
  <env name="AKHUB_LISTEN"              value="127.0.0.1:8080" />
  <env name="AKHUB_SHUTDOWN_GRACE_SECS" value="180" />
  <env name="RUST_LOG"                  value="akhub=info,warn" />

  <!-- 停止时先发 Ctrl+C，最多再等 240 秒才 TerminateProcess。
       默认只有 15 秒，会截断在途的长流式请求（§25.3）。 -->
  <stoptimeout>240sec</stoptimeout>
  <stopparentprocessfirst>true</stopparentprocessfirst>

  <onfailure action="restart" delay="2sec" />

  <logpath>C:\ProgramData\Akhub\logs</logpath>
  <log mode="roll-by-size">
    <sizeThreshold>10240</sizeThreshold>
    <keepFiles>8</keepFiles>
  </log>
</service>
```

安装与管理（管理员窗口里执行，命令就是可执行文件名本身）：

```
Set-Location 'C:\Program Files\Akhub'
.\akhub-service.exe install
.\akhub-service.exe start
.\akhub-service.exe status        # Started / Stopped / NonExistent
.\akhub-service.exe stopwait      # 停止并等到真的停了
.\akhub-service.exe uninstall
```

说明与限制：

- **`<stoptimeout>` 必须写。** 默认 15 秒就 `TerminateProcess`，比 NSSM 的默认值更狠；
  `240sec` 与 §4.2 同一个口径。值由 `ConfigHelper.ParseTimeSpan` 解析：先 `Trim()`，
  再按后缀匹配，`sec`/`secs` 都按「秒」算（`{ "sec", 1000 }`），所以 `240sec` 是 240 秒、
  `240 sec` 同样成立；**不写后缀时才是按毫秒解释**（`<stoptimeout>240</stoptimeout>` 只有 0.24 秒）。
  官方示例本身写的也是带空格的 `<stoptimeout>15 sec</stoptimeout>`。
  停止过程中 WinSW 会按 `<waithint>`（默认 15 秒）周期向 SCM 报 `SERVICE_STOP_PENDING`，
  所以 240 秒的等待不会被 SCM 当成挂死。
- WinSW 同样通过控制台 Control-C 触发优雅关闭，效果与 NSSM 等价。
- 服务账户默认 LocalSystem。要换账户用 XML 里的 `<serviceaccount>`，
  子元素是 `<domain>` / `<user>` / `<password>`（**不是** `<username>`，v2 的 schema 里没有这个名字），
  例如 `<serviceaccount><domain>.</domain><user>akhub</user><password>…</password></serviceaccount>`；
  或在注册后用 `sc.exe` 改（注意 `obj=` 与 `password=` 的等号后必须有空格）：
  `sc.exe config Akhub obj= ".\akhub" password= "YourPasswordHere"`。改完记得按 §4.1 授权。
- 日志文件名固定为 `akhub-service.out.log` / `akhub-service.err.log`（取 exe 基名）；Akhub 的日志在 `.err.log` 里。

### 4.4 验证“停止不会截断长流”

这条验收值得单独做一次，因为它正是 Windows 侧最容易错的地方：

1. 从任意兼容客户端发一个会长时间持续输出的流式请求（长思考的对话，或直接用 `curl` 打开一个 SSE 长连接）。
2. 保持连接不断，在另一个窗口执行 `nssm stop Akhub`（或 `Stop-Service Akhub`）。
3. 期望现象：服务日志出现「收到停止信号，正在等待在途请求完成 grace_secs=180」，已经开始的流继续正常输出到自然结束，客户端**不会**看到连接被重置；随后进程自己在几秒内退出。
4. 若流被立刻切断，回头检查 `AppStopMethodConsole`（NSSM）或 `<stoptimeout>`（WinSW）是否真的写进去了。

盯服务日志：

```
Get-Content 'C:\ProgramData\Akhub\logs\akhub.err.log' -Tail 50 -Wait
```

---

## 5. Windows 防火墙

**默认配置下什么都不用开。** Akhub 默认 `AKHUB_LISTEN`=`127.0.0.1:8080`，只绑环回地址：别的机器根本连不上（不是被防火墙拦，而是没有在可路由地址上监听），所以不存在需要放行的入站连接；环回流量也不受入站规则影响。

只有当你主动把 `AKHUB_LISTEN` 改成 `0.0.0.0:8080`（或某个内网 IP）时，才需要放行，并且尽量把来源收紧到可信网段：

```
New-NetFirewallRule -DisplayName 'Akhub 网关 8080' `
    -Direction Inbound -Action Allow -Protocol TCP -LocalPort 8080 `
    -RemoteAddress 10.0.0.0/8, 192.168.0.0/16 -Profile Domain, Private
```

用完删除：

```
Remove-NetFirewallRule -DisplayName 'Akhub 网关 8080'
```

取舍：即使只是给内网其他机器用，也**优先让 Caddy 监听可路由地址并反代到 `127.0.0.1:8080`**，而不是把 Akhub 直接暴露到网卡上——§6 的理由同样适用（明文 HTTP、后台凭据、无 TLS）。

---

## 6. 反向代理终止 HTTPS（Caddy for Windows）

**为什么不能让 Akhub 裸奔在公网：**

1. **明文的**：Akhub 只监听 HTTP，自身不管证书（§25.2）。管理后台的登录口令、网关的 Bearer Key 都会以明文经过网络。
2. **会话 Cookie 缺 `Secure`**：登录 Cookie 是否带 `Secure` 属性，取决于请求头 `X-Forwarded-Proto: https`；直连明文 HTTP 时这个头不存在，也就不会有 `Secure` 属性（这正是本地开发能直接登录的原因）。
3. **后台就是密钥库**：`/admin` 能增删上游账号与 API Key，拿到后台等价于拿到全部上游凭据。
4. 生产上的正确形态是：Caddy 监听 443 并自动申请证书，反代到 `127.0.0.1:8080`；Akhub 继续只绑环回。

Caddy 在 Windows 上是单个 exe，无需额外组件。

```
$CaddyVer = '2.11.4'
Invoke-WebRequest -UseBasicParsing `
    -Uri "https://github.com/caddyserver/caddy/releases/download/v$CaddyVer/caddy_$($CaddyVer)_windows_amd64.zip" `
    -OutFile "$env:TEMP\caddy.zip"
Expand-Archive "$env:TEMP\caddy.zip" -DestinationPath 'C:\Caddy' -Force
& 'C:\Caddy\caddy.exe' version
```

发行包里已经随附一份参考配置 `deploy\Caddyfile.windows`（含证书邮箱、日志文件、`read_timeout`/`write_timeout 600s` 与 `request_body max_size 64MB`），可以直接拷过来改域名，也可以按下面的最小写法自己写。若确实要用 Nginx，仓库里还有 `deploy\nginx.conf`，但 Windows 版 Nginx 是实验性构建，且必须确认 `proxy_buffering off` 与超时足够大，生产上仍推荐 Caddy。

新建 `C:\Caddy\Caddyfile`：

```
gw.example.com {
	reverse_proxy 127.0.0.1:8080
}
```

- 只写域名即可，Caddy 会自动申请并续期 Let's Encrypt 证书（前提是公网能访问本机 80/443，且域名解析到本机）。
- **不要为了“更实时”加 `flush_interval -1`**：Caddy 默认对 `Content-Type: text/event-stream` 和长度未知的响应就是即时透传，Akhub 的 SSE 不需要额外配置；而 -1 会连带关闭“客户端断开就取消上游请求”的行为，反而让 Akhub 继续给一个死掉的连接烧上游 token。
- 反代会自动补上 `X-Forwarded-Proto`，这正是后台 Cookie 拿到 `Secure` 所需要的；Caddy 会忽略客户端伪造的同名头。

配置校验与前台试跑：

```
& 'C:\Caddy\caddy.exe' adapt --config 'C:\Caddy\Caddyfile' --validate
& 'C:\Caddy\caddy.exe' run --config 'C:\Caddy\Caddyfile'    # 前台跑，确认证书签发成功再转服务
```

放行 80/443：

```
New-NetFirewallRule -DisplayName 'Caddy HTTP/HTTPS' -Direction Inbound -Action Allow `
    -Protocol TCP -LocalPort 80,443
```

**托管 Caddy 服务**，两条都行：

- Caddy 官方文档给的 `sc.exe` 写法（Caddy 自己实现了服务协议，所以这里 `sc.exe` 是合法的；路径里不要有空格，官方示例也用 `C:\Caddy`）：

```
  sc.exe create caddy start= auto binPath= "C:\Caddy\caddy.exe run --config C:\Caddy\Caddyfile"
  sc.exe start caddy
```

  注意 `start=` 与 `binPath=` 的等号后**必须**有空格。

- 或者复用 §4.3 的 WinSW：把 `WinSW-x64.exe` 重命名为 `C:\Caddy\caddy-service.exe`，配 `caddy-service.xml`（`<executable>%BASE%\caddy.exe</executable>`、`<arguments>run --config %BASE%\Caddyfile</arguments>`），并同样设置 `<stoptimeout>240sec</stoptimeout>`——默认 15 秒会把还在传输的长流掐掉。

小提示：Caddy 的证书与状态默认写在当前账户的 `%AppData%\Caddy`（以 LocalSystem 运行时是 `C:\Windows\System32\config\systemprofile\AppData\Roaming\Caddy`）。要让状态落到一个可备份的固定位置，给它设环境变量 XDG_DATA_HOME=`C:\ProgramData\Caddy`（WinSW 用 `<env>` 即可只对服务生效）。

---

## 7. 升级与回滚

**升级前必做一件事：在管理后台「设置 → 配置备份」导出一次配置备份**（口令加密，含账号与配置，不含请求记录）。它和 `master.key` 是两条独立的退路。

原则：**停服务之后再备份**。SQLite 在 WAL 模式下，运行中复制出来的 `akhub.sqlite` 可能是不一致快照，别把这种备份当救生圈。

```
$Tag   = 'v1.1.2'
$Bin   = 'C:\Program Files\Akhub\akhub.exe'
$Data  = 'C:\ProgramData\Akhub'
$Stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$Bk    = "C:\ProgramData\Akhub-backup-$Stamp"

# 1) 停服务（正常停止，最长等 240 秒把在途流放完）
Stop-Service Akhub
(Get-Service Akhub).WaitForStatus('Stopped', '00:06:00')

# 2) 备份二进制 + 整个数据目录（master.key 与 akhub.sqlite 都在里面）
New-Item -ItemType Directory -Force -Path $Bk | Out-Null
Copy-Item $Bin "$Bk\akhub.exe.bak" -Force
robocopy $Data "$Bk\data" /E /COPY:DAT /R:2 /W:1 /NFL /NDL /NJH /NJS
"备份在 $Bk"

# 3) 校验并替换新二进制
$Asset = "akhub-$Tag-windows-x86_64.zip"
# ……此处按 §1 下载并用 checksums.txt 校验 $Asset……
Expand-Archive -Path ".\$Asset" -DestinationPath . -Force
Copy-Item ".\akhub-$Tag-windows-x86_64\akhub.exe" $Bin -Force
& $Bin --version

# 4) 启动并等待就绪
Start-Service Akhub
$env:AKHUB_LISTEN = '127.0.0.1:8080'      # --healthcheck 按这个地址探测 /health/ready
$ok = $false
foreach ($i in 1..60) {
    & $Bin --healthcheck
    if ($LASTEXITCODE -eq 0) { $ok = $true; break }
    Start-Sleep -Seconds 1
}
if (-not $ok) { throw "升级后健康检查失败，执行回滚：$Bk" }
'升级成功'
```

**回滚**（健康检查失败或服务起不来时）：

```
Stop-Service Akhub
(Get-Service Akhub).WaitForStatus('Stopped', '00:06:00')

# 先把数据目录还原回升级前：新版本可能已经把库迁移到更高的结构版本，
# 旧二进制打开新库会被直接拒绝（§27：数据库结构版本高于二进制认识的版本）。
robocopy "$Bk\data" $Data /MIR /COPY:DAT /R:2 /W:1 /NFL /NDL /NJH /NJS

Copy-Item "$Bk\akhub.exe.bak" $Bin -Force
Start-Service Akhub
& $Bin --healthcheck; "exit=$LASTEXITCODE"
```

两个必须知道的点：

- **旧二进制打不开新数据库是设计行为，不是故障。** 结构版本随启动自动校验，用更旧的二进制打开更新的库会被拒绝并提示升级，不会写坏数据。正因为如此，回滚二进制时通常要连数据目录一起回滚；只回滚 exe 会卡在上面那条校验上。
- 还原数据目录也会把请求记录和配置倒回备份时刻，升级到回滚之间在后台做的配置修改会丢失——这也是升级前先导出配置备份的原因。

不保留旧版数据、只想换二进制时，第 3 步到第 4 步就够；但把 `master.key` 与 `akhub.sqlite` 放进同一个备份目录是硬要求。

---

## 8. 常见问题

### 8.1 端口被占用 / 明明没人监听却起不来

看谁占着：

```
Get-NetTCPConnection -LocalPort 8080 -State Listen |
    Select-Object LocalAddress, LocalPort, OwningProcess
Get-Process -Id (Get-NetTCPConnection -LocalPort 8080 -State Listen).OwningProcess
```

Akhub 绑定失败时日志里是「监听 `127.0.0.1:8080` 失败: <原因>」。处理方式二选一：停掉占用者，或把 `AKHUB_LISTEN` 换成别的端口——换端口时**三处要一起改**：服务的环境变量、`--healthcheck` 探测时的 `$env:AKHUB_LISTEN`、以及 Caddy 的 reverse_proxy 上游地址。

还有一种 Windows 特有的“假占用”：端口没有被进程监听，但落在 WSL2 / Hyper-V（`winnat`）预留的动态端口区间里，报错不是“已被占用”而是“访问权限不允许（`WSAEACCES` 10013）”。确认：

```
netsh int ipv4 show excludedportrange protocol=tcp
```

若 8080 落在某个区间里，要么换端口，要么把它先永久占下来再启动服务：

```
net stop winnat
netsh int ipv4 add excludedportrange protocol=tcp startport=8080 numberofports=1 store=persistent
net start winnat
```

### 8.2 数据目录权限不足，`master.key` 创建失败

日志形如「创建主密钥文件失败：`C:\ProgramData\Akhub\master.key`」并附带“拒绝访问”（`os error 5`）；同类错误还有「创建数据目录失败：…」「打开数据库失败：…`akhub.sqlite`」。

排查顺序：

```
# 1) 服务实际用哪个账户跑
(Get-CimInstance Win32_Service -Filter "Name='Akhub'").StartName

# 2) 该账户对数据目录到底有什么权限
icacls 'C:\ProgramData\Akhub'

# 3) 按 §4.1 补齐授权后重启
icacls 'C:\ProgramData\Akhub' /grant:r 'NT AUTHORITY\LocalService:(OI)(CI)M' /T
Restart-Service Akhub
```

同时记住这条不可逆的规则：**`master.key` 丢失后，数据库里加密保存的上游 API Key 无法恢复。** 进程发现文件不存在会生成新的一把，此时旧密文全部解不开，只能把上游 Key 重新录一遍。所以：数据目录永远和 `master.key` 一起备份；不要把数据目录权限放开成“人人可写”；换服务账户之前先确认新账户能读到它。

### 8.3 服务启动即退出，或注册完根本起不来

Akhub 的日志全部写到 **stderr**，所以先看错误日志（NSSM 路径）：

```
Get-Content 'C:\ProgramData\Akhub\logs\akhub.err.log' -Tail 80
```

WinSW 路径是 `C:\ProgramData\Akhub\logs\akhub-service.err.log`。

日志为空或根本没有这个文件时：`AppStdout` / `AppStderr` 指向的**目录不存在**会让 NSSM 直接启动失败（先 `New-Item -ItemType Directory -Force -Path 'C:\ProgramData\Akhub\logs'`）。再核对参数与事件日志：

```
& 'C:\Program Files\Akhub\nssm.exe' get Akhub AppEnvironmentExtra
& 'C:\Program Files\Akhub\nssm.exe' get Akhub AppExit

# 事件查看器 → Windows 日志 → 应用程序，来源 nssm / Service Control Manager
Get-WinEvent -LogName Application -MaxEvents 30 |
    Where-Object { $_.ProviderName -match 'nssm|Service Control Manager' } |
    Select-Object TimeCreated, ProviderName, Id, Message
```

最常见的几类原因，以及日志里的原文：

| 日志关键字 | 原因 | 处理 |
| --- | --- | --- |
| `AKHUB_LISTEN` 不是合法的监听地址 | 环境变量的值不是合法的 `IP:端口` | 用 `127.0.0.1:8080` 或 `0.0.0.0:8080` |
| `AKHUB_MASTER_KEY` 必须是 32 字节的 hex 或 base64 值 | 环境变量长度或编码不对 | 32 字节 = 64 个 hex 字符，或 base64 形式 |
| 监听 … 失败 | 端口被占或落在保留区间 | 见 §8.1 |
| 创建主密钥文件失败 / 打开数据库失败 | 权限 | 见 §8.2 |
| 没有任何 Akhub 日志 | 进程压根没被拉起来 | 检查 `Application` 路径、`AppDirectory`、服务账户 |

另外两条排查手段：

- 用**服务账户**在交互式窗口里跑一次前台（§3），能直接看到错误原文，比翻日志快；账户不同导致的差异也能一次暴露。
- 目标机若弹出「无法启动此程序，因为计算机中丢失 `VCRUNTIME140.dll`」，装一次 Microsoft Visual C++ 2015–2022 可再发行组件（x64）即可，不需要别的运行时。

### 8.4 Windows Defender / SmartScreen 误报未签名二进制

现状要说清楚：**Akhub 的 Windows 二进制没有代码签名**（发布流程里没有签名步骤），`Get-AuthenticodeSignature` 会显示 `NotSigned`。因此从浏览器下载后可能出现「Windows 已保护你的电脑」，Defender 也可能对“刚从网上下来的、不带签名、还会开监听端口的 exe”给出可疑提示。处理口径：

1. **先验证来源，再决定放行。** 按 §1 核对 SHA256 与 `checksums.txt` 一致；哈希对得上再谈放行，对不上就删掉重下，不要点“仍要运行”。
2. SmartScreen 提示里走「更多信息 → 仍要运行」。用脚本或自动化下发时，先去掉文件上来自 Internet 的标记：

```
   Unblock-File 'C:\Program Files\Akhub\akhub.exe'
   Get-Item 'C:\Program Files\Akhub\akhub.exe' -Stream * | Select-Object Stream
```

3. **不要为了这个关掉 Defender 的全盘防护。** 企业环境里按最小面放行，两个选项：
   - 路径/进程排除（适合已固定安装位置）：

```
     Add-MpPreference -ExclusionPath 'C:\Program Files\Akhub'
     Add-MpPreference -ExclusionProcess 'C:\Program Files\Akhub\akhub.exe'
```
   - 更严格的做法是用 `New-CIPolicy` / AppLocker 基于文件哈希建立允许规则；或者由企业代码签名证书给 exe 签名后重新分发。
4. 确认是误报（而不是真的被投毒）时，向 Microsoft 提交误报样本走官方申诉，比长期加排除项更干净。
5. 顺带降低误报面：把 exe 放在 `C:\Program Files\Akhub` 而不是用户目录或临时目录，也别放在被同步盘同步的路径下。

---

## 9. 与 Docker / WSL2 的关系

不想装服务时，Windows 上同样可以走 **Docker Desktop**（直接 `docker compose up -d`，默认就把端口绑在 `127.0.0.1:8080`，停止宽限已在编排文件里设成 200 秒）或 **WSL2**（按 Linux 原生那一套跑）。两种方式的数据卷/目录、备份与停止超时要求完全一样。

Docker 与 Linux 原生的完整步骤见仓库里的 [`deploy/README.md`](README.md) 和 [`deploy/akhub.service`](akhub.service)。

---

参考：

- 优雅关闭与停止超时：计划文档 §25.3；Linux 侧对应 `deploy/akhub.service` 的 `TimeoutStopSec=200`，容器侧对应 `docker-compose.yml` 的 `stop_grace_period: 200s`。
- 环境变量全集与默认值见项目 `README.md`；`akhub.exe` `--help` 会打印最常用的一批。
- `akhub.exe` `--healthcheck` 探测的是本机 `/health/ready`（数据库、配置、主密钥都就绪才返回 0），适合放进任何监控或滚动脚本；`/health/live` 只表示进程存活，`/health/version` 不暴露任何凭据地给出当前版本号。