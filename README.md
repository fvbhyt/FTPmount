# ftpmount-rs — 用 Rust 重写 FTP 挂载

> ⚠️ **本文写于 2026-10-18（0.1.0 时期），部分结论已被后续实验推翻。**
> 接手前请先读 [`HANDOFF.md`](HANDOFF.md)，其中第六节列出了本文已过时的内容。
> 以下说法**不再成立**：「覆盖/删除被沙箱拦住，与本工具无关」——真因是 WinFsp 磁盘模式的
> `err=5`，0.1.3 已改用网络模式修复；「时间精度靠 MDTM 逐项补齐」——目标服务器不支持 MDTM。
> 以 `CHANGELOG.md` 和 `HANDOFF.md` 为准。

替代 `rclone + PowerShell 壳` 的方案：自己实现 FTP 客户端 + WinFsp 文件系统回调，
把时间、缓存、上传时机全部握在自己手里。

**当前状态**：读 + 写都已实现，并在真机挂载上验证通过。
新建 / 写入 / 上传 / 改名 / 建目录 / 删目录 / 大文件全部端到端跑通。
覆盖已有文件和删除文件这两条路径在本机被沙箱策略拦住，**已用 WinFsp 官方 memfs 做过对照，
两者表现完全一致**，确认与本工具无关（详见「环境限制」）。

| 指标 | RaiDrive | rclone 版 | **ftpmount-rs** |
| --- | --- | --- | --- |
| 体积 | 44 MB（两个 exe） | 82 MB | **2.7 MB** |
| 内存 | 106 MB | 48 MB | **14.9 MB** |
| 组件 | 服务 + 界面 + CBFS 驱动 | rclone.exe | 1 个 exe |
| 时间精度 | 精确 | LIST 无时区时偏 8h | **精确（MDTM 逐项补齐）** |

## 实测结果

只读（本地 FTP，127.0.0.1:2121）：

```
root: 9 items
mtime 2024-03-15 10:30:00 size 15     ← 与服务器真实时间一致
read ok: 'hello from ftp\n'
sub: ['nested.txt']
```

FTP 写路径自检：

```
[1] STOR /_rs_selftest.txt -> 28 bytes
[2] RETR 回读 28 bytes, 内容一致=true        ← 含 \x00\x01\x02 二进制尾部
[3] MKD /_rs_selftest_dir ok
[3] 列表里能看到目录=true
[4] RNFR/RNTO ok，旧名还在=false 新名存在=true
[5] 清理后残留文件=false 残留目录=false
```

挂载点上的端到端验收（`acceptance.py`，每步都与服务器核对）：

| 操作 | 客户端 | 服务器 | 结论 |
| --- | --- | --- | --- |
| 新建 + 写入 | ok | 内容一致 | 通过 |
| 改名 | ok | 旧名消失、新名存在、内容保留 | 通过 |
| 建目录 | ok | 目录出现 | 通过 |
| 目录内写文件 | ok | 目录内出现、内容一致 | 通过 |
| 删空目录 | ok | 目录消失 | 通过 |
| 删非空目录 | 报「目录不是空的」 | 目录保留 | 通过 |
| 读回校验（md5） | 一致 | — | 通过 |
| 覆盖已有文件 | 被沙箱拒绝 | 未改动 | 环境限制 |
| 删除文件 | 被沙箱拒绝 | 未改动 | 环境限制 |

大文件（256 MB，本地 FTP 回环）：

```
写入  256 MB   5.37 s   47.7 MB/s   挂载进程内存峰值 19.5 MB（增长 0）
读取  256 MB   2.34 s  109.2 MB/s   挂载进程内存峰值 19.3 MB（增长 6.7 MB）
服务器收到 268435456 字节，与源文件一致
```

内存不随文件大小增长，是因为写走「本地缓存文件 + 关闭时整体 STOR」，
读走「边收边写缓存文件」，两边都不把文件内容整个读进内存。

> 早期版本读路径用 `retr_as_buffer` 把整个文件读进 `Vec` 再落盘，
> 256 MB 文件峰值到过 514.9 MB。改成 `retr()` 流式写盘后降到 19.3 MB，吞吐也从 84.7 提到 109.2 MB/s。

## 用法

```bash
cargo build --release

# 自检：解析配置 + 连服务器 + 列根目录 + 报告 MLSD 支持情况
ftpmount-rs.exe check test.toml

# 写路径自检：上传/回读/建目录/改名/删除（不需要 WinFsp）
ftpmount-rs.exe selftest test.toml

# 挂载
ftpmount-rs.exe mount test.toml R:
```

配置（TOML，兼容现有 `ftpmount.conf` 的 `[ftp]` 段落）：

```toml
[ftp]
host = "127.0.0.1"
port = 2121
user = "test"
pass = "test"
drive = "R:"
volname = "FTP Drive"
list_tz_offset = 480   # 仅当服务器既无 MLSD 也无 MDTM 时用于修正 LIST 时间
```

## 写是怎么做的

不做"每次 write 都发一次 STOR"，而是**本地缓存 + 关闭即上传**：

```
create  →  本地建空缓存文件，登记到 pending（远端还没这个文件）
write   →  只写本地缓存，标 dirty
close   →  cleanup 里一次性 STOR 上传，然后删掉缓存
```

这样解决了三件事：

1. **"指定的设备不存在"** —— 建文件不再等 FTP 往返，Explorer 的创建时序不会被打断。
2. **写大文件不卡** —— 连续写只打本地磁盘，最后一次上传。
3. **半截文件不上服务器** —— 上传发生在关闭时，不会把写了一半的内容传上去。

配套的几个细节：

- `pending` 表让只在本地、尚未上传的文件也能被 stat 和列目录看到，否则资源管理器里刚建的文件会消失。
  **反过来**：pending 里的文件必须能被打开，否则「打不开 → 删不掉」，
  资源管理器里会出现一个删不掉的幽灵文件。`begin_open` 对 pending 文件不再去远端下载。
- 新建后不写内容也要上传（空文件）：卷参数设了 `post_cleanup_when_modified_only`，
  WinFsp 会跳过 cleanup，空文件就永远传不上去。
- `upload()` 失败时保留 pending，让文件仍然可见、仍然能删，别凭空消失。
- `rename` 改的是 Handle 里的路径（`Mutex<String>`），否则后续上传会传到旧名字。
- 文件删除：`set_delete` 只打标记，真正 `DELE` 在 `cleanup` 里；从没上传过的新文件直接丢缓存，不发 DELE。
- 目录删除：`set_delete` 里**立刻** `RMD`。因为 `cleanup` 的返回值 WinFsp 拿不到，
  放那里会出现「目录删不掉但系统显示成功」。删非空目录时把 550 翻译成
  `STATUS_DIRECTORY_NOT_EMPTY`，资源管理器才会提示"文件夹不为空"。
- 读：`open` 时用 `retr()` 流式写入缓存文件，不把整个文件读进内存。
- 连接登录后强制 `TYPE I`（二进制），否则 ASCII 模式会改 CRLF，写二进制文件必坏。

## 时间为什么能准

三级降级，逐级补齐：

1. **MLSD** — `modify=20240315103000`，本来就是 UTC，最准。
2. **MDTM** — 服务器不支持 MLSD 时，对 LIST 出来的每一项单独问一次，拿 UTC 时间。
3. **LIST** — 只有这个可用时，用 `list_tz_offset` 把服务器本地时间换算成 UTC。

第 3 级是兜底：LIST 对半年以上的文件只给日期不给时分秒，精度必然丢失，
但至少时区是对的——这正是 rclone 做不到的那一步。

## 踩过的三个 WinFsp 坑（都花了时间，记下来）

### 1. 不能设 `VolumeParams.prefix`

Prefix 非空会让 WinFsp 走 `WinFsp.Net` 设备，那需要 Launcher 服务在跑；
否则 `FspFileSystemCreate` 直接返回 `0xD000000D`（无效参数），连卷都建不起来。
盘符挂载点用 `host.mount("R:")` 设置，走 `WinFsp.Disk`。

### 2. `open` 必须填 `OpenFileInfo` 里的 FileInfo

WinFsp 内核层判断"这个句柄是不是目录"，靠的是 **Open 返回的 FileInfo 里的
`FILE_ATTRIBUTE_DIRECTORY`**。不填的话：

- 目录枚举直接被拒（`STATUS_INVALID_PARAMETER` / WinError 87）
- `ReadDirectory` 回调**根本不会被调用**，日志里一条都看不到

这是最难查的一个——现象是"读文件正常，列目录报参数错误"，而代码看起来完全没问题。

```rust
let fi: &mut FileInfo = file_info.as_mut();
fill_info(fi, &meta);
```

### 3. 不需要装 WinFsp SDK

`winfsp-sys` 这个 crate 自带 `winfsp-x64.lib` 和头文件，
本机只装了 WinFsp 运行时（`C:\Program Files (x86)\WinFsp\bin`，没有 `lib/`、`inc/`）也能编过。
配合 `delayload` 特性 + `build.rs` 里一行 `winfsp::build::winfsp_link_delayload()`，
运行时再去加载 DLL。

## 代码结构

```
src/
├── main.rs     # CLI（check / selftest / mount），WinFsp 初始化、卷参数、挂载
├── config.rs   # TOML 配置，兼容 [ftp] 段落
├── ftp.rs      # FTP 客户端（suppaftp）+ MLSD/LIST/MDTM 解析 + STOR/RETR/DELE/MKD/RMD/RNFR
└── fs.rs       # FileSystemContext 实现（open/create/overwrite/write/read/cleanup/rename/set_delete）
```

`fs.rs` 分两层：核心逻辑（`begin_open` / `begin_create` / `do_write` / `do_cleanup` …）
只返回 `Result<_, u32>`，不碰 WinFsp 类型；`impl FileSystemContext` 只做参数转换。
这样逻辑可以单独测，也方便以后换挂载层。

测试脚本：

```
acceptance.py   挂载点端到端验收，每步与服务器核对（读写改名建删目录）
bench_big.py    大文件写入：耗时 + 挂载进程内存峰值
bench_read.py   大文件读取：耗时 + 挂载进程内存峰值
```

## 还没做的

- **覆盖写 / 删除文件的真机补测**：本机被沙箱策略拦住（见下节），需要在干净机器上验证一遍。
- **FTPS**：`tls = true` 还没接（`suppaftp` 的 `RustlsConnector` 已经编进来了）。
- **按需分块读**：`open` 时会把整个文件先拉到本地缓存。内存已经不涨了，
  但 1 GB 文件仍要占 1 GB 本地磁盘、并且打开时要等完整下载。应该改成读到哪下到哪。
- **断线重连**：出错丢弃连接下次重连，但没有重试次数和退避。
- **目录缓存**：`stat()` 每次都 LIST 父目录，大目录会很慢。
  → **2026-10-18 已做**：`list()` 加了 1 秒 TTL 缓存，任何改动会让全部缓存失效；
  同时去掉了「无条件逐项 MDTM」的 N² 往返（只有 LIST 行里取不到时间的条目才补 MDTM）。
- **根目录时间**：`\` 的时间戳显示 1970（LIST 不给根目录 mtime），可以补一次 MDTM。
- **目录枚举的 marker 语义**：**2026-10-18 已修复**。读了 WinFsp `dirbuf.c` 源码核实
  marker 找不到时是从「第一个比 marker 大的条目」接着走、不会从头重来（早先「疑似
  `.` / `..」的猜想是错的）。偶发重复条目的真因是 crate 的 `set_name` 把 NUL 算进了
  `DirInfo.Size`，导致 marker 查找系统性地落空一条——详见「80% CPU / 卡死事故」一节。
- **小文件读取缓存**：**2026-10-18 已做**。`ftp.rs` 新增 `retr_to_file_cached()`：≤1 MiB 的文件
  内容在 2 秒 TTL 内命中内存，重复打开同一文件不再走 FTP；大文件始终流式落盘不进缓存。
  任何写操作（`stor`/`dele`/`mkd`/`rmd`/`rename`）成功后 `invalidate_all()` 同时清空
  列表缓存与读缓存。实测：本地回环 ftp-srv 上因进程启动+Defender 约 350ms 常量占主导，
  小文件缓存**看不出挂钟收益**；但在高 RTT 的远程 FTP 上每次重复打开省一个完整 RTT，
  价值在此兑现（见下节实测数据）。
- **GUI**：暂时用命令行。

## 环境限制：覆盖写与删除文件无法在本机验证

两条路径在本机复现为 `ERROR_ACCESS_DENIED`，但**不是本工具的问题**：

1. 对已有文件做 `CREATE_ALWAYS`（`>` 重定向、Python `open(w)`、`printf` 都算）
2. 删除文件（`DeleteFile` / `FILE_FLAG_DELETE_ON_CLOSE` / `FileDispositionInformation` 三条路都一样）

排查过程与证据：

- 请求根本没进到文件系统回调。开着 `FT_DEBUG=1` 看不到 `overwrite` / `set_delete` 日志，
  而**删除目录**的 `set_delete` 日志正常出现并成功 RMD，说明回调接线是对的。
- 用 WinFsp 安装包自带的参考实现 `memfs-x64.exe` 挂到 `S:` 做对照，**表现完全一致**：
  第一次建文件成功，第二次覆盖失败；删除文件同样 `err=5`，删除目录同样成功。
- 打开 `memfs -d -1` 的调试日志，覆盖请求连 `>>Create` 都没有打印——
  说明在进入用户态文件系统之前就被拒了。
- 最后沙箱自己报了原因：`Blocked by Security Center rules: write \Device\Volume{GUID}\...`。
  策略按设备路径生效，对所有虚拟卷一视同仁，所以 memfs 和我们一起被拦。

结论：本机无法验证这两条路径，需要在一台没有该策略的机器上补测。
代码侧这两条路径的实现与 WinFsp 官方示例一致（`Overwrite` 回调 + `SetDelete` 回调）。

## 环境事故（2026-10-18）

挂载验证到一半，`C:\Program Files (x86)\WinFsp` 整个目录、注册表项、
`System32\drivers\winfsp-x64.sys` 全部消失。不是代码导致的——同一时刻 rclone 也报
`cgofuse: cannot find winfsp`。

已把安装包下到 `D:\tools\winfsp-2.1.25156.msi`（2.19 MB，OLE 头校验通过），
但沙箱不允许静默安装系统组件（`msiexec` 返回 83），需要手动双击安装。

## 80% CPU / 卡死事故：根因已找到并修复（2026-10-18）

用户报障：挂载后进程 CPU 打到 80%，机器基本卡死，只能重启。**根因已定位并修复**。
复测：40 个文件的目录 `ls -la` 从「永久挂死」到 1.25 秒完成，空闲 CPU 0%。

### 根因：winfsp crate 的 `DirInfo::set_name` 把结尾 NUL 算进了 `Size`

`read_directory` 填 DirBuffer 时，crate 的 `set_name`/`set_name_cstr` 会把名字连同
结尾 NUL 一起写入缓冲区，并把 NUL 也算进 `DirInfo.Size`（实测：6 字符名 Size=118 =
头部 104 + 7×2，第 7 个字符就是 NUL）。

而 WinFsp 的 marker 二分查找（`src/dll/dirbuf.c`）用
`(Size - sizeof(FSP_FSCTL_DIR_INFO))/2` 当名字长度，再和消费者回传的 marker 比较：

```
前缀相等后：res = alen - blen = +1   // 条目恒比 marker「长 1 个字符」
```

查找必然落空并回退一条，**把 marker 指向的条目本身又返回给消费者**。消费者
（msys/Explorer）反复处理同一个文件名，枚举永远推进不下去 —— 「同一个条目重复输出
上百次」「ls -la 卡死」「进程 CPU 30~80%」三个症状是同一个根因。

**修复**：`fs.rs` 的 `read_directory` 改用 `set_name_cstr` 写入后，手工把 NUL 占的
2 字节从 `Size` 字段减掉（`Size` 位于 DirInfo 偏移 0，与 `FSP_FSCTL_DIR_INFO` 同一套
C ABI，跨版本稳定；`set_name_raw` 在 crate 的 sealed 模块里，外部用不了）。

### 连带挖出的另外三个缺陷（都已修）

| 缺陷 | 症状 | 修复 |
|---|---|---|
| 路径大小写：查找时忽略大小写能找到文件，但发 FTP 命令用的是**请求时的大小写**，而 FTP 服务器大小写敏感 | `\STRESS2\F9.TXT` 这类查询 550 → 调用方不停重试 | `fs.rs` 新增 `resolve()`：逐级用服务器 LIST 返回的真实名字拼路径；Handle 增加 `remote_path`，RETR/STOR/DELE/RMD/RNFR 全走它 |
| stat 风暴：Explorer 枚举时对**每个文件**触发一次 stat，每次都真去 LIST 父目录，MLSD 不通时还逐项补 MDTM | N 个文件 = O(N²) 次 FTP 往返，全串行在一条 mutex 连接上 | `ftp.rs` 的 `list()` 加 1 秒 TTL 缓存（任何写操作后整体失效）；MDTM 只在 LIST 行取不到时间时才补 |
| FTP 层零超时：所有操作共用一条 mutex 连接，服务器一挂回调永远阻塞 | 整个盘僵死、进程空转烧 CPU（实测 26%） | 控制连接 + PASV 数据连接全部设读写超时（建连 10s / IO 15s）；失败即丢弃连接，下次自动重连 |
| **写入内容损坏**：`begin_open` 的 pending 分支照抄了 `begin_create` 的 `created=true`。于是「只读打开一个尚未上传完的文件」在 cleanup 时会把自己那份**空**本地缓存 `STOR` 回服务器，把创建句柄刚传好的内容覆盖成 0 字节 | 刚写入 18 字节的文件立刻被并发打开（Defender / msys 重试）覆盖成空文件；读回 0 字节 | pending 分支改为 `created=false`（打开来的句柄永远不负责上传，上传只由创建句柄负责，其失败由 `do_close` 的「尚未上传，补传」兜底）；删除判定同时看 `pending` 表而非只看 `created`，避免误发 DELE；`do_write` 同步 pending 条目的大小，让并发读取报正确大小 |

**服务器崩溃实测**：操作 30 秒内干净报错（不再永久挂死），期间 CPU 0%；服务器恢复后
**不重挂载**自动重连，`cat` 0.56 秒读通。

### 其他实测数据（40 个文件的目录）

- `ls -la` 1.25s（冷缓存）；30 次连续列举 45s；10 轮 find+stat 16.4s
- 单次真实 LIST 8-21ms（本机 ftp-srv）；缓存生效时每目录每秒最多 1 次真实 LIST
- 剩余瓶颈：每个文件仍被打开 4 次（3 次属性 + 1 次大写名读数据），属性打开已不落盘；
  绝对耗时主要在 FSD IRP 往返（50 次 stat 的系统时间占 6.2s）
- 空闲 CPU 0%，内存 ~19 MB（峰值受缓存上限 64 MiB 约束）；`fstest` 11/11 通过
- **读缓存后补测（2026-10-18）**：
  - 同文件连读 5 次：~340-410ms/次，日志显示 8 次内存命中 / 2 次下载——缓存生效；
    但挂钟时间被进程启动+Defender 常量吃掉，本地回环看不出差别（预期内）。
  - 400KB / 1.5MB 大文件流式：与服务器 md5 逐字节一致；>1 MiB 不进缓存（日志 `大文件不进缓存`）。
  - **写后读一致性**：刚写入 18 字节的新文件立刻读回，内容与写入一致，日志 `upload size=0`
    出现 0 次（修复前会污染成 0 字节）。
  - **改后即时可见**：`put` 改写某文件后同一会话内 `cat` 立即看到新内容，说明写失效生效。

### 环境限制（非本工具问题，未改动）

- 覆盖/追加写已有文件、bash `rm` 删除文件：本机沙箱策略拦截（memfs 对照复现过；
  bash 的 `rm` 是被 safe-delete 包装层拦的，genie-trash 无法规范化虚拟盘路径）。
  **资源管理器里的删除/新建/改名均正常**（用户已实测，日志实证）。
- 本机 ftp-srv 首个 STOR 偶发 550 EPERM（文件被瞬时锁，疑似 Defender），
  close 时的补传逻辑可兜住。

## 依赖

- Rust 1.99（MSVC 工具链）
- WinFsp 2.1 运行时
- crates：`winfsp` 0.13（`delayload`）、`suppaftp` 12（`rustls-ring`）、`chrono`、`serde`、`toml`、`anyhow`
