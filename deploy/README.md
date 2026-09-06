# Linux 原生部署（§25.2）

适合不使用 Docker、由 systemd 管理的单机部署。

## 1. 安装二进制

从发布产物（`dist/akhub-<target>.tar.gz`，见 `scripts/release.sh`）或自行构建：

```bash
cargo zigbuild --release --target x86_64-unknown-linux-gnu   # 或 aarch64-...
sudo install -m755 target/x86_64-unknown-linux-gnu/release/akhub /usr/local/bin/akhub
```

## 2. 创建系统用户与数据目录

```bash
sudo useradd --system --home-dir /var/lib/akhub --create-home akhub
```

## 3. 安装 systemd 单元

```bash
sudo install -m644 deploy/akhub.service /etc/systemd/system/akhub.service
sudo systemctl daemon-reload
sudo systemctl enable --now akhub
```

单元文件已经做了三件重要的事：

- `Restart=always`：单实例下进程故障是常态恢复路径，重启代价应压到数秒。
- `TimeoutStopSec=200`：优雅关闭给在途长流式请求留足完成时间（§25.3），
  必须大于 `AKHUB_SHUTDOWN_GRACE_SECS`。
- `ProtectSystem=strict` / `StateDirectory=akhub`：只允许写自己的数据目录。

## 4. 验证

```bash
curl -s http://127.0.0.1:8080/health/ready   # 200 = 数据库、配置、主密钥就绪
sudo /usr/local/bin/akhub --healthcheck      # 容器外复用同一段探测逻辑
sudo journalctl -u akhub -f
```

## 5. 备份

数据目录里只有两样东西需要备份：`akhub.sqlite`（配置与请求元数据）和
`master.key`（主密钥）。**主密钥丢失后库里的上游 Key 无法恢复。** 也可以在
管理后台「设置 → 配置备份」导出入口令加密的配置备份（不含请求记录）。

## 升级

1. `sudo systemctl stop akhub`（SIGTERM，等待在途请求完成）。
2. 替换二进制。
3. `sudo systemctl start akhub`。

数据库结构版本随启动自动校验：用**更旧的**二进制打开**更新的**数据库会被
拒绝并提示升级（§27），不会写坏数据。
