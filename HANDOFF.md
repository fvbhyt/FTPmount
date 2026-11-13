# 接手导读（HANDOFF）

> 给第一次接触这个项目的人 / AI。**先读这份建立全局认知，再读 `README.md`（功能说明）和 `CHANGELOG.md`（版本演进）。**
> 最后更新：2026-11-11 · 对应版本 **v0.1.12**

---

## 一、这是什么

把 FTP 服务器挂成 Windows 本地盘符（如 `Z:`）。自己实现 FTP 客户端 + WinFsp 文件系统回调，不依赖 rclone / RaiDrive。

- **语言**：Rust（核心库 + Tauri 2 桌面端）
- **运行时依赖**：WinFsp（第三方内核驱动，**必须装**）
- **规模**：约 6000 行 Rust（`src/` 核心库 + `desktop/` 桌面端）
- **当前版本**：v0.1.12

已验证可用：新建 / 写入 / 读取 / 改名 / 建目录 / 删目录 / 覆盖（弹框确认）/ 大文件流式传输，全部端到端跑通（`acceptance.py` + `cov.py`）。

---

## 二、仓库内容与代码地图

```
ftpmount-rs/
├── src/                     ← 核心库（CLI 与桌面端共用）
│   ├── fs.rs                ← WinFsp 文件系统回调，全部业务逻辑在这里
│   ├── ftp.rs               ← 连接池、命令封装、LIST/MLSD 解析、读缓存、超时与熔断
│   ├── main.rs              ← CLI 入口：参数、挂载、日志初始化
│   ├── lib.rs               ← 模块声明 + WinFsp 加载 + 挂载模式开关
│   ├── logbuf.rs            ← 日志：环形缓冲 + 按天轮转（14 天自动回收）+ 实时回调
│   └── config.rs            ← TOML 配置结构
├── desktop/                 ← Tauri 2 桌面端（GUI）
│   ├── src/                 ← 前端：index.html / app.js / style.css（原生 JS，无框架、无 npm）
│   └── src-tauri/
│       ├── src/main.rs      ← Tauri 入口、窗口、托盘、单实例、命令分发
│       ├── src/cmds.rs      ← 暴露给前端的命令（增删站点、挂载、卸载、连接测试、读日志）
│       ├── src/mgr.rs       ← 挂载生命周期管理（后台线程 + 停止信号）
│       ├── src/store.rs     ← 站点配置持久化（JSON，口令用 DPAPI 加密）
│       ├── src/dpapi.rs     ← 口令加解密（Windows DPAPI）
│       ├── src/keepawake.rs ← 防锁屏守护线程（见第五节）
│       └── tauri.conf.json  ← 打包配置（identifier: com.ftpmount.desktop）
├── examples/                ← 独立诊断程序（bigread / rename_probe）
├── acceptance.py            ← 挂载点端到端验收（每步与服务器核对）
├── cov.py                   ← 回归测试：10 项断言，逐项对比 WinFsp CreateDisposition 语义
├── bench_big.py / bench_read.py ← 大文件耗时与内存峰值
├── gui.ps1                  ← 纯 PowerShell + WinForms 的轻量控制台（零依赖，可选）
├── myftp.example.toml       ← 连接配置脱敏模板（复制为 myftp.toml 使用）
├── gen_icon.py              ← 图标占位生成脚本（正常不需要跑）
└── build.rs / Cargo.toml / Cargo.lock
```

> **以下三类是本地工作区概念，不进仓库（clone 后没有，不必找）**：
> - `_work/`：临时脚本与备份（含真实凭据备份，**不入库**）
> - `cankao/`：搬迁前的旧版快照，仅供本地对照
> - `notes/` / `_release/`：排错记录与历史安装包
>
> 排错的「已定性结论」已浓缩在本文第四、五、十节，不依赖外部文件。

---

## 三、核心数据模型（看懂这三个就能改代码）

### 1. `FtpFs` 的三个共享状态

| 字段 | 作用 | 何时动 |
|---|---|---|
| `pending` | 只在本地存在、还没上传的文件 | `create` 写入时加，上传完成后删 |
| `mtime_over` | 本地时间戳覆盖表（路径 → FILETIME） | 服务器不支持 `MFMT` 时，记住客户端想要的真实时间 |
| `notify_q` | 待上报的目录变更事件 | 资源管理器靠它把进度推到 100% |

### 2. `FileState` 的三个关键标志（**改这里最容易出事**）

| 标志 | 含义 | 约束 |
|---|---|---|
| `whole` | 缓存内容是否完整 | `false`=只读惰性打开，按区间 `REST` 补拉；`true`=读直接走本地 |
| `fresh` | 是否整份重写远端文件 | **只有 `fresh=true` 才允许开流式上传**（开流会先截断远端） |
| `stream` | 正在进行的流式上传 | `Some`=写入直接发 STOR，关闭时不需要整文件回传 |

**三者关系是最易踩的坑**：「打开已有文件改几个字节」的句柄 `whole=true` 但 `fresh=false`，若给它开流只写开头一块，远端文件就被截短。

### 3. 读写路径

```
读：create/open(只读) → whole=false, ranges=[]
    read(offset) → 命中 ranges 读本地；否则 retr_range(REST) 补拉
    小文件（≤1MiB）走内存缓存，大文件流式落盘
写：create/open(写) → 建空缓存（不下载！）
    write → 写本地缓存；若 fresh 则同时开 StreamUp 直接 STOR
    close → 没开流的，整文件 stor() 回传
```

**写打开必须惰性下载**：`FILE_OVERWRITE_IF` 的内核路径是 Open(写)→Overwrite，若一打开就整文件 `RETR`，Overwrite 紧接着又截成 0 —— 覆盖大文件会白下整份。

---

## 四、五个关键设计决策（改之前先看）

1. **覆盖要弹框 → `Create` 必须返回 `STATUS_OBJECT_NAME_COLLISION (0xC0000035)`**。WinFsp 的 `Create` 回调拿不到 `CreateDisposition`（内核消费掉）。资源管理器先拿 `FILE_CREATE` 试建一次，收到 COLLISION 才弹「替换/跳过」。把「已存在」当「直接覆盖」就永远不弹。（0.1.3 已修）

2. **6 秒干等无解**。盘符被识别为 `DRIVE_REMOTE` 时，资源管理器每次操作后多发两次属性探测再干等。要去掉只能让盘符是 `DRIVE_FIXED`，但这台机器 `WinFsp.Disk` 写打开 `err=5`。两者互斥，框架内无解。**不要再去修。**

3. **所有 FTP 命令必须用 `Handle.remote_path` 而不是请求路径**。Windows 侧大小写不敏感（`\STRESS2\F9.TXT`），FTP 服务器大小写敏感，拿请求路径发命令会 550。`remote_path` 是服务器 `LIST` 返回的真实大小写。

4. **超时四层**：建连 4s / 读写 6s / 单操作硬截止 8s / 失败熔断 3s。没有超时的后果实测过：ftp-srv 崩过一次，所有回调永远收不到响应，整个盘僵死烧 CPU。

5. **服务器能力探测**：优先 `MLSD`，失败退回 `LIST` 解析（`parse_list` 带时区偏移）。目标服务器**不支持 `SIZE`/`MDTM`/`MLSD`**，时间靠 `LIST` + `list_tz_offset` 修正。

---

## 五、防锁屏（keepawake）—— 第二个独立功能

与挂载**互不影响**。源码 `desktop/src-tauri/src/keepawake.rs`，配置 `keepawake.json`。**（0.1.10 引入，不是 0.1.2）**

**这是干什么的**：Windows 默认无操作几分钟后关闭显示器、甚至进入睡眠。挂上 FTP 盘后长时间不碰键盘鼠标（看大文件、等传输），系统休眠可能让 FTP 连接中断、盘「假死」。它调用 Windows 的 `SetThreadExecutionState` 告诉系统「我还醒着」，从而**阻止关闭显示器和睡眠**。

**它聪明在不是无脑常亮**：一份进程清单（比如某播放器、股票软件，或 `explorer.exe` / FtpMount 自身），清单里**任意一条「已启用」的程序正在运行 → 屏幕常亮**；它们全退出了 → **立刻恢复**正常休眠。另有「**全局强制常亮**」开关：不看清单，直接常亮。

**怎么配**：
- 桌面端界面里从「正在运行的程序」下拉挑，或手输进程名（如 `explorer.exe`）；可单条启用 / 停用 / 删除。
- 落盘 `keepawake.json`：优先放 **exe 同级目录**（便携版方便手改），装在 `Program Files` 写不进去时回落 `%APPDATA%\FtpMount\keepawake.json`（界面底部显示实际路径）。
- 手改 json 不用重启：守护线程每 2 秒看一次文件 mtime，有改动就重载。

**状态看得见**：托盘菜单红点 = 常亮中、灰点 = 未触发，并列出「现在是谁在触发」；主窗口标题栏的「FtpMount」字样在常亮时变红；防锁屏页签徽标显示「常亮中 · 谁触发」。

**安全边界**：用的是**线程级**状态，由活着的线程周期性续命，进程退出就自动失效，**不会留下「永久常亮把屏幕烧着」**。

**挡不住的**（任何工具都做不到，写清楚免得以后当 bug）：手动 `Win+L` 锁屏、组策略「计算机不活动限制」、动态锁（手机离开蓝牙范围）。

---

## 六、构建与运行（clone 后第一步看这节）

### 前置依赖

| 项 | 是否必需 | 说明 |
|---|---|---|
| Rust（MSVC 工具链，stable） | 必需 | `rustup` 装 `stable-msvc` |
| WinFsp 2.x 运行时 | 挂载必需 | 装运行时即可；编译不需要 SDK |
| WebView2 | 桌面端运行必需 | Win11 一般自带；否则装 Evergreen 运行时 |
| `tauri-cli` | 构建桌面端必需 | `cargo install tauri-cli --version "^2"`（全局） |
| Visual Studio Build Tools | 编译必需 | 「使用 C++ 的桌面开发」工作负载 |
| 本地 FTP 服务器 | 跑测试必需 | 如 `pyftpdlib`，用于 `acceptance.py` / `cov.py` |

> 前端是**纯静态 HTML/JS，无 npm、无 Node.js**，不要为了构建桌面端去装 Node。

### CLI（核心库）

```bash
cargo build --release
# 产物：target/release/ftpmount-rs.exe
ftpmount-rs.exe check myftp.example.toml      # 测连通 / 可写 / MLSD 支持
ftpmount-rs.exe selftest myftp.example.toml   # 写路径自检（不需要 WinFsp）
ftpmount-rs.exe mount myftp.example.toml R:    # 挂载（需要管理员）
```

### 桌面端（Tauri 2）

```bash
cd desktop/src-tauri
cargo tauri dev     # 开发模式，前端热重载
cargo tauri build   # 打包，产物在 target/release/bundle/nsis/FtpMount_0.1.12_x64-setup.exe
```

---

## 七、怎么验证改动

```bash
# 1. 编译
cargo build --release
cd desktop/src-tauri && cargo tauri dev      # 或 cargo check

# 2. 回归测试（CreateDisposition 语义，10 项断言）
python cov.py Z:                              # 需先挂载

# 3. 端到端验收（自己挂载、测试、卸载，每步与服务器核对）
python acceptance.py

# 4. 看日志
#    %APPDATA%\FtpMount\logs\ftpmount-YYYY-MM-DD.log
#    关键标记：[慢] [失败] [卡住] [忙] [空闲] [通知]
```

**判断「逻辑坏了」之前先看日志是不是 `os error 10060`**：服务器偶发连接超时，stream 断掉后 upload 失败，远端文件被 `STOR` 截成 0 字节，后续读到的全是 NUL。重跑就好。

---

## 八、发版规矩

版本号末位 +1，**三处同步改**（`Cargo.toml`、`desktop/src-tauri/Cargo.toml`、`desktop/src-tauri/tauri.conf.json`），同时更新 `CHANGELOG.md`。
打包（需先 `cargo install tauri-cli`）：`cd desktop/src-tauri && cargo tauri build`。

---

## 九、环境要求

| 项 | 说明 |
|---|---|
| 系统 | Windows 10 / 11 |
| Rust | stable-msvc |
| WinFsp | 2.x 运行时 |
| WebView2 | 桌面端窗口（系统自带或 Evergreen 运行时） |
| `tauri-cli` | 全局 `cargo install tauri-cli --version "^2"` |
| VS Build Tools | 「使用 C++ 的桌面开发」工作负载 |

---

## 十、已知限制与已否决方向

- **FTPS**：`tls = true` 的接入口已留好（`suppaftp` 的 `RustlsConnector` 已编入），但未实装联调。
- **按需分块读**：打开大文件会先整份拉到本地缓存（内存已不涨，但占本地磁盘且打开时要等完整下载），计划改成读到哪下到哪。
- **6 秒**：盘符 `REMOTE` 身份导致，框架内无解（见第四节 2）；日常挂载建议用 RaiDrive 作替代。
- **CfAPI 重写**：曾评估用 Cloud Files API 让产物是真实 NTFS 目录 + 占位文件，从根上绕开 err=5 和 6 秒；但占位符元数据一旦损坏（`WinError 363`）文件就变成删不掉、读不开的僵尸，需要停系统级过滤驱动才能清，**已否决**这条路线。要「盘符像本地硬盘」仍只有文件系统驱动（WinFsp / Dokan）一条路。
