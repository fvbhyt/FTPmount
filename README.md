# FtpMount — 把 FTP / FTPS 服务器挂载成 Windows 本地盘符

用 Rust + WinFsp 自己实现 FTP 客户端和文件系统回调，把 FTP 服务器挂成一个像本地硬盘一样的盘符（如 `Z:`），
在资源管理器里直接拖拽、读写。配套一个 Tauri 2 写的中文桌面端，以及「防锁屏」小工具。

| 指标 | RaiDrive | rclone 版 | **FtpMount** |
| --- | --- | --- | --- |
| 体积 | 44 MB（两个 exe） | 82 MB | **约 2.7 MB** |
| 内存 | 106 MB | 48 MB | **约 15 MB** |
| 组件 | 服务 + 界面 + 驱动 | rclone.exe | **1 个 exe** |
| 时间精度 | 精确 | LIST 无时区时偏 8h | **精确（三级降级修正）** |

> 本文对应 **v0.1.12**。安装包：`FtpMount_0.1.12_x64-setup.exe`（NSIS，简体中文，内嵌 WinFsp 运行时）。

---

## 功能特性

### 一、FTP 挂载（核心）

- **读写完整**：新建、写入、改名、建目录、删目录、读回校验全部端到端跑通；覆盖已有文件时资源管理器会弹「替换 / 跳过」确认框（不会静默覆盖）。
- **文件时间精确**：三级降级 —— 优先 `MLSD`（UTC 最准）→ 不支持时逐项 `MDTM` → 只有 `LIST` 时按 `list_tz_offset` 把服务器本地时间换算成 UTC。这正是 rclone 做不到的那一步。
- **大文件不占内存**：写走「本地缓存 + 关闭即整体上传」，读走「流式落盘」，两边都不把整个文件读进内存。256 MB 文件挂载进程内存峰值约 19 MB，吞吐 100+ MB/s。
- **不卡不死**：目录列举带 1 秒 TTL 缓存；FTP 连接全部设读写超时（建连 10s / IO 15s），服务器挂掉时干净报错、恢复后自动重连，不永久挂死、不烧 CPU。
- **半截文件不上传**：上传只发生在文件关闭时，不会把写了一半的内容传上服务器；上传失败保留本地记录，文件仍可见、仍可删。

### 二、桌面端（Tauri 2 图形界面）

- **多站点管理**：可保存多个 FTP 连接（服务器 / 端口 / 账号 / 远端起始目录 / 盘符 / 时区），一键切换。
- **连接测试**：不挂载就能测连通性、报告延迟、可写性、是否支持 MLSD。
- **一键操作**：挂载 / 卸载 / 打开盘符 / 读写自检，状态卡片实时显示「已挂载 / 挂载中 / 失败」。
- **凭据本地加密**：密码用 Windows DPAPI 加密后存到 `%APPDATA%\FtpMount\config.json`，不在磁盘上留明文。
- **托盘 + 单实例**：常驻系统托盘；重复打开只唤起已运行的窗口，不会起多个挂载进程撞车。
- **开机自启 + 最小化到托盘**：可设开机自动挂载指定站点；关窗口默认缩到托盘而非退出。
- **实时日志面板**：前端攒帧渲染（拷贝大文件涌进几十行时也不卡 UI），可清空、可打开日志目录。
- **浅色 / 深色主题**，以及「挂载到 `ftp://host:port/path → 盘符`」的实时预览，填地址时一眼看清最终连哪。

### 三、防锁屏（keepawake） ← 0.1.10 引入，0.1.11 / 0.1.12 打磨

> 你记成 0.1.2 了，实际是 **0.1.10** 加的（CHANGELOG 可查）。它是挂载之外的「第二个功能」，两者互不影响。

**这是干什么的**：Windows 默认在无操作几分钟后关闭显示器、甚至进入睡眠。挂上 FTP 盘后，如果你长时间不碰键盘鼠标（看大文件、等传输），系统休眠可能让 FTP 连接中断、盘「假死」。防锁屏就是让程序运行时调用 Windows 的 `SetThreadExecutionState`，告诉系统「我还醒着」，从而**阻止关闭显示器和睡眠**。

**它聪明在不是无脑常亮**：

- 你给一份「进程清单」（比如某播放器、股票软件，或直接填 `explorer.exe` / FtpMount 自身）。清单里**任意一条「已启用」的程序正在运行 → 屏幕常亮**；它们全退出了 → **立刻恢复**正常休眠。
- 另有「**全局强制常亮**」开关：不看清单，直接常亮（适合你人走开但想让盘一直活着的场景）。

**怎么配**：

- 界面里从「正在运行的程序」下拉挑，或手输进程名（如 `explorer.exe`）；可单条启用 / 停用 / 删除。
- 配置落盘到 `keepawake.json`：**优先放 exe 同级目录**（便携版方便手改），装在 `Program Files` 写不进去时回落到 `%APPDATA%\FtpMount\keepawake.json`（界面底部显示实际路径）。
- 手改 json 不用重启：守护线程每 2 秒看一次文件 mtime，有改动就重载。

**状态看得见**：托盘菜单里红点 = 常亮中、灰点 = 未触发，并列出「现在是谁在触发」；主窗口标题栏的「FtpMount」字样在常亮时变红；防锁屏页签右上角徽标显示「常亮中 · 谁触发」。

**安全边界**：用的是**线程级**状态，由活着的线程周期性续命，进程退出就自动失效，**不会留下「永久常亮把屏幕烧着」**。

**挡不住的**（任何工具都做不到，写清楚免得以后当 bug）：手动 `Win+L` 锁屏、组策略「计算机不活动限制」、动态锁（手机离开蓝牙范围）。

### 四、安装与打包

- NSIS 安装包，简体中文，安装模式 perMachine。
- 安装包**内嵌 WinFsp 2.1 运行时 msi**：首次安装若检测到没装 WinFsp，会自动装上（挂载的底层依赖）。
- 单 exe，无后台服务、无需管理员常驻（仅挂载盘符那一刻需要提权）。

---

## 安装与使用

### 桌面端（推荐）

1. 运行 `FtpMount_0.1.12_x64-setup.exe`，按向导装好（会自动确保 WinFsp 已安装）。
2. 启动 FtpMount，在「连接」页填服务器信息 → 点「连接测试」→ 通过后「挂载」。
3. 资源管理器里出现对应盘符，像本地磁盘一样用。
4. 「防锁屏」页签里按需添加要监控的程序，或开「强制常亮」。

### 命令行 / 开发

```bash
cargo build --release

# 解析配置 + 连服务器 + 列根目录 + 报告 MLSD 支持（不需要管理员）
ftpmount-rs.exe check myftp.example.toml

# 写路径自检：上传 / 回读 / 建目录 / 改名 / 删除（不需要 WinFsp）
ftpmount-rs.exe selftest myftp.example.toml

# 挂载到盘符（需要管理员）
ftpmount-rs.exe mount myftp.example.toml R:
```

配置（复制 `myftp.example.toml` 改成你的，详见文件内注释）：

```toml
[ftp]
host = "192.168.1.10"   # 服务器域名或 IP
port = 21
user = "yourname"
pass = "yourpassword"
remote_path = ""        # 留空 = 根目录；填 "/public" 只挂这个子目录
drive = "Z:"
volname = "My FTP"
list_tz_offset = 480    # 北京/上海时区 +480；服务器在 UTC 上填 0
tls = false
max_conn = 4
```

---

## 技术要点

**写是怎么做的**：不做「每次 write 都发一次 STOR」，而是本地缓存 + 关闭即上传。
`create → 本地建空缓存 → write 只写本地 → close 时一次性 STOR → 删缓存`。
解决三件事：建文件不等 FTP 往返（不打断 Explorer 时序）、写大文件不卡、半截文件不上服务器。
配套：`pending` 表让未上传文件也能被看到；空文件也会上传；`rename` 改的是 Handle 路径；
目录删除在 `set_delete` 里立刻 `RMD`（非空目录翻译成 `STATUS_DIRECTORY_NOT_EMPTY` 让资源管理器提示）。

**时间为什么准**：MLSD → MDTM → LIST + `list_tz_offset` 三级降级，逐级补齐时区。

**代码结构**：

```
src/                      # Rust 核心（CLI + WinFsp 回调 + FTP 客户端）
├── main.rs               # CLI（check / selftest / mount）、WinFsp 初始化、卷参数
├── config.rs             # TOML 配置
├── ftp.rs                # FTP 客户端（suppaftp）+ MLSD/LIST/MDTM 解析 + STOR/RETR/DELE/MKD/RMD/RNFR
├── fs.rs                 # FileSystemContext 实现（open/create/overwrite/write/read/cleanup/rename/set_delete）
└── logbuf.rs             # 日志缓冲（按天滚动，14 天自动回收）

desktop/                  # Tauri 2 桌面端
├── src/                  # 前端（原生 JS，无框架，最小化内存）
└── src-tauri/
    ├── src/
    │   ├── main.rs       # 窗口 / 托盘 / 单实例 / 命令分发
    │   ├── cmds.rs       # 挂载 / 卸载 / 连接测试 / 读写自检等命令
    │   ├── mgr.rs        # 挂载进程管理
    │   ├── store.rs      # 多站点配置（DPAPI 加密凭据）
    │   ├── dpapi.rs      # Windows DPAPI 加解密
    │   └── keepawake.rs  # 防锁屏守护线程
    └── tauri.conf.json   # 打包配置（identifier: com.ftpmount.desktop）
```

**测试脚本**：`acceptance.py`（挂载点端到端验收，每步与服务器核对）、`bench_big.py` / `bench_read.py`（大文件耗时与内存峰值）。

---

## 依赖

- Rust（MSVC 工具链）
- WinFsp 2.1 运行时（安装包内已附带）
- crates：`winfsp` 0.13（`delayload`）、`suppaftp` 12（`rustls-ring`）、`chrono`、`serde`、`toml`、`anyhow`、`tauri` 2（`tray-icon` / `autostart` / `single-instance`）、`windows` 0.61

## 已知限制

- **FTPS**：`tls = true` 的接入口已留好（`suppaftp` 的 `RustlsConnector` 已编入），但当前版本尚未实装联调。
- **按需分块读**：打开大文件会先把整个文件拉到本地缓存（内存已不涨，但 1 GB 文件会占 1 GB 本地磁盘、且打开时要等完整下载）。后续计划改成读到哪下到哪。
- **覆盖写 / 删除文件的真机补测**：需在干净机器上验证（本机有安全策略拦截这两条路径，已用 WinFsp 官方 memfs 对照确认与本工具无关）。
