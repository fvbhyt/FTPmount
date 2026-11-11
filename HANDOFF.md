# 接手导读（HANDOFF）

> 给第一次接触这个项目的人 / AI。**先读这一份，再读别的。**
> 最后更新：2026-10-22

---

## 一、这是什么

把 FTP 服务器挂成 Windows 盘符（`Z:`）。自己实现 FTP 客户端 + WinFsp 文件系统回调，
不依赖 rclone / RaiDrive 之类的外部程序。

- **语言**：Rust（CLI 核心库 + Tauri 2 桌面端）
- **运行时依赖**：WinFsp（第三方内核驱动，必须装）
- **规模**：5728 行 Rust，其中 `src/fs.rs` 2350 行是主体
- **当前版本**：0.1.9（挂载行为等同 0.1.3）

### 已经做到的

新建 / 写入 / 读取 / 改名 / 建目录 / 删目录 / 删除文件 / 覆盖已有文件 / 大文件流式传输，
全部端到端验证通过（`acceptance.py` + `_work/cov.py`）。

### 已知缺陷（不是 bug，是框架限制）

**拷贝小文件会在 99% 卡约 6 秒。** 原因是 WinFsp 网络模式下盘符被资源管理器识别为
`DRIVE_REMOTE`，每次操作后多发两次属性探测再干等。**这个已定性为无解，不要再去修**
（详见第五节和 `notes/ftpmount-排错全记录.md`）。

---

## 二、先读什么（按这个顺序）

| 顺序 | 文件 | 为什么 |
|---|---|---|
| 1 | 本文件 | 建立全局认知 |
| 2 | `CHANGELOG.md` 最后 3 个版本 | 最近改了什么、为什么改 |
| 3 | `src/lib.rs`（110 行） | 模块划分 + 挂载模式的完整说明（注释里写透了） |
| 4 | `src/fs.rs` 第 1~500 行 | `FtpFs` / `Handle` / `FileState` / `StreamUp` 的定义与注释 |
| 5 | `src/ftp.rs` 第 1~100 行 | 缓存策略、超时与熔断的取值理由 |
| 6 | `README.md` | **注意：部分结论已过时，见第六节** |

**不要**从头通读 `fs.rs`。它按 WinFsp 回调顺序排（create → open → read → write → cleanup → close），
按需跳到对应回调即可。

---

## 三、代码地图

```
ftpmount-rs/
├── src/                     ← 核心库（CLI 与桌面端共用）
│   ├── fs.rs     2350 行    ← WinFsp 文件系统回调。全部业务逻辑在这里
│   ├── ftp.rs     897 行    ← 连接池、命令封装、LIST/MLSD 解析、读缓存
│   ├── main.rs    617 行    ← CLI 入口：参数、挂载、日志初始化
│   ├── logbuf.rs  282 行    ← 日志：环形缓冲 + 按天轮转 + 实时回调
│   ├── lib.rs     110 行    ← 模块声明 + WinFsp 加载 + 挂载模式开关
│   └── config.rs   74 行    ← TOML 配置结构
│
├── desktop/                 ← Tauri 2 桌面端
│   ├── src/                 ← 前端：index.html / app.js / style.css（原生 JS，无框架）
│   └── src-tauri/
│       ├── src/main.rs  253 ← Tauri 入口、托盘、单实例
│       ├── src/cmds.rs  405 ← 暴露给前端的命令（增删站点、挂载、卸载、读日志）
│       ├── src/mgr.rs   341 ← 挂载生命周期管理（后台线程 + 停止信号）
│       ├── src/store.rs 256 ← 站点配置持久化（JSON）
│       └── src/dpapi.rs 143 ← 口令加解密（Windows DPAPI）
│
├── examples/                ← 独立诊断程序（bigread / rename_probe）
├── cankao/ftpmount-rs/      ← 2026-10-19 搬迁前的旧版快照，仅供对照，不要改
├── _work/                   ← 临时脚本与备份，任务结束清空
├── acceptance.py            ← 挂载点端到端验收（每步与服务器核对）
├── _work/cov.py             ← 回归测试：10 项断言，逐项对比 WinFsp memfs 行为
└── _release/                ← 历史安装包 0.1.0 ~ 0.1.9
```

### `fs.rs` 内部结构

| 区域 | 行号 | 内容 |
|---|---|---|
| `timed()` / `Inflight` / `spawn_stall_watchdog()` | 31~150 | 计时、在飞请求计数、卡住告警 |
| `FtpFs` 定义 | 150~168 | 三个共享状态（见第四节） |
| `Handle` / `StreamUp` / `FileState` | 192~360 | 每个打开句柄的状态机 |
| `impl FtpFs` | 493~1698 | 内部方法（缓存、上传、区间下载、通知） |
| `impl FileSystemContext for FtpFs` | 1745~2236 | **WinFsp 回调本体**，按需查阅 |
| `impl NotifyingFileSystemContext` | 2237~2264 | 目录变更通知推送 |
| 缓存工具 / 错误映射 | 2265~2350 | `drop_cache_file` / `purge_cache` / `map_ftp_err` |

---

## 四、核心数据模型（看懂这三个就能改代码）

### 1. `FtpFs` 的三个共享状态

| 字段 | 作用 | 什么时候动它 |
|---|---|---|
| `pending` | 只在本地存在、还没上传的文件 | `create` 写入时加，上传完成后删 |
| `mtime_over` | 本地时间戳覆盖表（路径 → FILETIME） | 服务器不支持 `MFMT`，上传后 mtime 会变成上传时刻，这里记住客户端想要的真实时间 |
| `notify_q` | 待上报的目录变更事件 | 资源管理器靠它把进度推到 100%，收不到就等 6 秒兜底超时 |

### 2. `FileState` 的三个关键标志（**改这里最容易出事**）

| 标志 | 含义 | 约束 |
|---|---|---|
| `whole` | 缓存内容是否完整 | `false` = 只读惰性打开，按区间 `REST` 补拉；`true` = 读直接走本地 |
| `fresh` | 是否整份重写远端文件 | **只有 `fresh=true` 才允许开流式上传**。因为流是一条顺序 `STOR`，开流等于先把远端文件截断 |
| `stream` | 正在进行的流式上传 | `Some` = 写入直接发往 STOR，关闭时不需要整文件回传 |

**这三者的关系是整个项目最容易踩的坑**：
「打开已有文件改几个字节」的句柄是 `whole=true` 但 `fresh=false` —— 如果给它开了流，
只写开头一块，远端文件就被截短了。0.1.4 之前踩过。

### 3. 读写路径

```
读：create/open(只读) → whole=false, ranges=[]
    read(offset) → 命中 ranges 就直接读本地；否则 retr_range(REST) 补拉
    小文件（≤1MiB）走内存缓存，大文件流式落盘

写：create/open(写) → 建空缓存（不下载！）
    write → 写本地缓存；如果 fresh 则同时开 StreamUp 直接 STOR
    close → 没开流的，整文件 stor() 回传
```

**写打开必须惰性下载**：`FILE_OVERWRITE_IF` 的内核路径是 Open(写) → Overwrite，
如果一打开就整文件 `RETR`，Overwrite 紧接着又截成 0 —— 覆盖 8GB 文件会白下 8GB。

---

## 五、五个关键设计决策（改之前先看这里）

1. **覆盖不弹框 → `Create` 必须返回 `STATUS_OBJECT_NAME_COLLISION (0xC0000035)`**
   WinFsp 的 `Create` 回调拿不到 `CreateDisposition`（内核消费掉了）。资源管理器先拿
   `FILE_CREATE` 试建一次，**收到 COLLISION 才弹「替换/跳过」**。把「已存在」当「直接覆盖」
   就永远不弹。已修（0.1.3）。

2. **6 秒不可修** —— 盘符身份问题，`WinFsp.Net` = REMOTE = 资源管理器多等。
   `WinFsp.Disk` 能给 FIXED，但这台机器上写打开 `err=5`。**两者互斥，框架内无解。**

3. **所有 FTP 命令必须用 `Handle.remote_path` 而不是请求路径**
   Windows 侧大小写不敏感（`\STRESS2\F9.TXT`），FTP 服务器大小写敏感，拿请求路径发命令只会 550。
   `remote_path` 是服务器 `LIST` 返回的真实大小写。

4. **超时三层**：连接 4s / 读写 6s / 单操作硬截止 8s + 失败熔断 3s。
   没有超时的后果实测过：ftp-srv 崩过一次，所有回调永远收不到响应，整个盘僵死烧 CPU。

5. **服务器能力探测**：优先 `MLSD`，失败退回 `LIST` 解析（`parse_list` 带时区偏移）。
   目标服务器**不支持 `SIZE` / `MDTM` / `MLSD`**，所以时间靠 `LIST` + `list_tz_offset` 修正。

---

## 六、⚠️ `README.md` 里已过时的结论

README 写于 2026-10-18，以下内容**已被后续实验推翻**，不要照它推理：

| README 里的说法 | 实际情况 |
|---|---|
| 「覆盖已有文件和删除文件被沙箱策略拦住，确认与本工具无关」 | 真因是 **WinFsp 磁盘模式的 `err=5`**，与沙箱无关。0.1.3 改用网络模式已修好 |
| 「时间精度：MDTM 逐项补齐」 | 目标服务器**不支持 MDTM**，实际靠 `LIST` + 时区偏移 |
| 「当前状态：读写都已实现」 | 那是 0.1.0 的状态，现在是 0.1.9，中间修了覆盖弹窗、惰性下载、日志反压等 |

**以 `CHANGELOG.md` 和本文件为准。**

---

## 七、下一步能做什么

| 方向 | 说明 | 代价 |
|---|---|---|
| **不动** | 日常用 RaiDrive 挂，这个工具留作备用 | 零 |
| **CfAPI 重写 I/O 层** | 产物是真实 NTFS 目录 + 占位文件，**不实现文件系统设备** → 从根上绕开 err=5 和 6 秒；`subst` 成盘符即 FIXED | `fs.rs` 2350 行作废，重写 1000+ 行；placeholder 管理复杂；subst 盘符对提权进程不可见 |
| 换 Dokan | Dokan 创建 disk 类设备，显示本地磁盘 | `dokan` crate **停在 2022-12**，新版要走 git 依赖；**同为 disk 类设备，大概率吃同样的 err=5** |
| 修 6 秒 | 只能改 WinFsp 驱动或改资源管理器行为 | 都不在我们手里 |

> 2026-10-22 查证更正：此前写的「ProjFS 有 `projected-fs` crate」是错的 —— 该 crate **不存在**。
> CfAPI / ProjFS 的底层绑定都在 `windows` crate（微软官方）或手写 `extern "system"`，
> 缺的是高层封装，不是没有绑定。**准确说法是「要自己写」，不是「生态弱」。**

**建议**：先想清楚「必须盘符 + 必须像本地硬盘」是不是硬要求。如果是，WinFsp 这条路已经证明
走不通（见 `notes/ftpmount-排错全记录.md` 第四节）；唯一能绕开的是 CfAPI，但要按新项目做。

---

## 八、改完之后怎么验证

```bash
# 1. 编译
cargo build --release
cd desktop/src-tauri && cargo check

# 2. 回归测试（10 项断言，逐项对比 WinFsp memfs 行为）
python _work/cov.py Z:          # 需要先挂载

# 3. 端到端验收（每步与服务器核对）
python acceptance.py            # 会自己挂载、测试、卸载

# 4. 挂载后手动看日志
#    %APPDATA%\FtpMount\logs\ftpmount-YYYY-MM-DD.log
#    关键标记：[慢] [失败] [卡住] [忙] [空闲] [通知]
```

**判断「逻辑坏了」之前先看日志里是不是 `os error 10060`** —— 服务器偶发连接超时，
stream 断掉后 upload 也失败，远端文件被 `STOR` 截成 0 字节，后续读到的全是 NUL。重跑就好。

### 发版规矩

版本号末位 +1，**三处同步改**（`Cargo.toml`、`desktop/src-tauri/Cargo.toml`、
`desktop/src-tauri/tauri.conf.json`），同时更新 `CHANGELOG.md`。
打包：`cd desktop/src-tauri && cargo tauri build`。

---

## 九、环境要求

| 项 | 版本 | 备注 |
|---|---|---|
| Rust | 1.99.0 | MSVC 工具链 |
| VS Build Tools | 2019 / 2022 | 装「使用 C++ 的桌面开发」 |
| Node.js | 22.x | 只有桌面端需要 |
| **WinFsp** | 2.2.26286 | **运行时必需**，2.1 在 Win11 26H2 上问题更多 |
| WebView2 | 系统自带 | 桌面端窗口 |

本机环境：Windows 11 专业版 build 26300。

**注意**：C 盘挂不了 WinFsp 卷（建 reparse point 报 `STATUS_OBJECT_NAME_COLLISION`），
D 盘可以。这与本项目无关，是系统环境特性。
