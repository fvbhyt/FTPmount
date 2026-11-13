# Syncthing 客户端集成评估（面向 ftpmount-rs 接手者）

> 评估日期：2026-11-13。目标：用 Rust 客户端替代 Windows 上的 SyncTrayzor，最终集成进 FtpMount 桌面端。
> **硬约束（用户明确）：替换 GUI 时现有配置必须接过来，不能重新同步。**
> 本文只写「归属、边界、接管清单、设计、风险、路线」，不写版本流水账。所有连接地址用配置项表达，不含任何真实设备信息或口令。

## 一、结论：配置不用「搬」，也就不会重新同步

这是本项目最重要的一个认知，先说清楚：

**文件夹、设备、共享关系、API Key、设备身份、文件索引，全部不属于 SyncTrayzor，
它们躺在 Syncthing 自己的 home 目录里，由 `syncthing.exe` 独占读写。**
SyncTrayzor 只是一个「启动器 + 托盘 + 内嵌浏览器」。换掉它，只要 daemon 仍指向同一个 home、
用同一个版本的 exe、保留同一套证书和索引数据库，重启后同步状态是原样延续的 `idle`，
不会重扫、不会重传、不需要重新配对设备。

所以「把配置接过来」在实际操作上不是导入/迁移，而是**复刻 SyncTrayzor 持有的三样启动资产**：

1. `syncthing.exe` 本体（见 3.3，卸载 SyncTrayzor 会带走它）
2. 启动参数 —— 实测只有 `--no-browser --no-restart`，**不要**加 `--gui-address`（见 3.2）
3. 读取 API Key 的正确路径（见 3.3，有两份同名 `config.xml`，别读错）

新 GUI 对配置的「管理」则通过 `/rest/config/folders`、`/rest/config/devices` 直接读写
同一份配置，不需要另建一套配置存储。**绝不能让 Rust 客户端自己维护一份配置副本**，
两份配置必然漂移。

**可行，且集成路径顺**：FtpMount 桌面端已是 Tauri 2 + 托盘 + 单实例 + 开机自启，
「替代 GUI」所需的壳它已经做好了 80%，真正要新增的只是一个 REST 客户端模块 + 一个页签。

## 二、边界：三层结构

| 层 | 是什么 | 谁提供 | 本次要不要动 |
|---|---|---|---|
| 同步引擎 + 配置/身份/索引 | BEP 协议、设备发现、块交换、版本控制、`config.xml`、`cert.pem`/`key.pem`、`index-v2` | `syncthing.exe`（Go） | **绝对不动** |
| 控制接口 | REST API（GUI 端口）+ 事件长轮询 + 官方 Web GUI | 同上，自带 | **对接，不改** |
| 桌面壳 | 进程托管、托盘图标、通知、内嵌浏览器、开机自启、日志窗口 | SyncTrayzor（.NET 4.7.2 / CefSharp） | **用 Rust 重写这一层** |

SyncTrayzor 官方仓库已标注 **unmaintained**（核心源码最后实质更新在 2021-08），
官方建议迁到 `GermanCoding/SyncTrayzor` v2。所以「换掉它」这个动机本身成立。

## 三、现有配置的真实归属（本机只读实测）

### 3.1 启动链与 home 目录

```
SyncTrayzor.exe -minimized
  └─ syncthing.exe --no-browser --no-restart --gui-address=<回环地址:端口>
       └─ syncthing.exe（实际监听 GUI 端口）
```

命令行里**没有 `--home`、没有 `--data`、没有 `--config`**，也没有环境变量覆盖，
所以用的是 Syncthing 默认 home：`%LOCALAPPDATA%\Syncthing\`
（已用 `GET /rest/system/paths` 复核）。

该目录内容：

| 文件 | 作用 | 动了会怎样 |
|---|---|---|
| `config.xml` | 文件夹、设备、共享关系、GUI 设置、API Key | 丢了要重新配置 |
| `cert.pem` / `key.pem` | **设备身份，Device ID 由它们决定** | 换了 = 换了设备，所有远端要重新配对 |
| `index-v2/` | 索引数据库（文件哈希与元数据） | 删了要全量重扫、重算哈希 |
| `https-cert.pem` / `https-key.pem` | GUI 的 HTTPS 证书（本机 `tls=false`，未用） | 无影响 |
| `syncthing.lock` / `syncthing.log` | 锁文件、日志 | 无影响 |

### 3.2 GUI 地址由 `config.xml` 自己决定，启动参数别乱加

`%LOCALAPPDATA%\Syncthing\config.xml` 的 `<gui address="...">` 是**具体回环地址 + 端口**，
不是 `dynamic`；SyncTrayzor 自己的配置里 `SyncthingCommandLineFlags` 为空，
daemon 日志也明确写着 `GUI and API listening (address=<config.xml 里的值>)`。
也就是说：**监听地址来自配置，不是来自启动参数**。

- **接管时只传 `--no-browser --no-restart`**，不要传 `--gui-address`。
  Syncthing 会把这个参数**写回 `config.xml`**，属于修改生产配置，
  与「接手但不改配置」的约束冲突；当前配置写的是具体地址，不传参数就能监听正确端口。
- **什么时候才需要 `--gui-address`**：只有当 `<address>` 是 `dynamic` 时，
  才说明旧壳曾用该参数覆盖。那种环境要先查清旧壳的实际参数再决定，
  不要照抄本节结论。
- API Key 固定在 `config.xml`，实测可用（见 3.4）。**每次启动都从该文件读，不要硬编码**。

顺带记一条容易误判的现象：旧壳弹「必须先终止 Syncthing 进程才能接管」，
说的是**进程托管权**（谁 spawn / 谁停止 daemon），不是 REST 端口被占用。
纯 REST 客户端只要端口上有一个活着的 daemon 就能连，不需要「接管」；
反过来，把唯一的 daemon 终止掉之后端口空着，会报 `10061 连接被拒绝`
—— **这是没服务监听，不是被占用**。终止之后必须有人把 daemon 重新拉起来。

### 3.3 三个容易踩的「撞名 / 会丢」陷阱

1. **两份同名 `config.xml`**：
   `%APPDATA%\SyncTrayzor\config.xml` 是 **SyncTrayzor 自己的 .NET 应用设置**，
   `%LOCALAPPDATA%\Syncthing\config.xml` 才是 **Syncthing 的配置**。解析目标只能是后者。
2. **`syncthing.exe` 会被卸载带走**：正在运行的是
   `%APPDATA%\SyncTrayzor\syncthing.exe`（约 34 MB，v2.1.7）；
   安装目录 `C:\Program Files\SyncTrayzor\syncthing.exe` 是**另一份旧副本**（约 27 MB），
   同目录还有 `syncthing.exe.old`。**接管前必须先把正在用的那个 exe 另存到 FtpMount 自己的位置**，
   或改用独立安装的同版本 Syncthing。卸载 SyncTrayzor 之前先做这件事。
3. **别拿旧 exe 开新 home**：两份 exe 版本不同，用旧版打开 `index-v2` 可能触发
   数据库迁移或全量重扫。**接管一律沿用正在运行的那个版本**，稳定后再单独讨论升级。

### 3.4 实证：REST 能完整读回现有配置（本次已验证）

用 `config.xml` 里的现有 API Key（只读 GET，未做任何写操作）：

| 检查 | 结果 |
|---|---|
| `GET /rest/system/paths` | 返回默认 home，与预期一致 |
| `GET /rest/config/folders` | 1 个文件夹，字段齐全（id / path / type / devices），类型 `sendonly`，共享给 2 台设备 |
| `GET /rest/config/devices` | 2 台设备，字段齐全 |
| `GET /rest/db/status?folder=...` | 状态 **`idle`**，全局/本地文件数一致（约 1.8 万），无需传输 |
| `GET /rest/system/status` | 设备 ID 存在 |

**这就是「配置接得过来」的证据**：新壳不需要任何配置导入，
第一次启动直接调这几个接口就能还原完整视图，且同步状态是已完成的。

## 四、无损接管清单（cutover checklist）

**必须原样保留**（一个都不能动）
- `%LOCALAPPDATA%\Syncthing\` 整个目录：`config.xml`、`cert.pem`、`key.pem`、`index-v2\`
- 设备 ID —— 由证书决定，接管前后必须一致
- 文件夹 ID 与共享关系 —— 由 `config.xml` 决定

**必须复刻**
- 同一个 `syncthing.exe`（版本、文件本体）
- 启动参数 `--no-browser --no-restart`（不加 `--gui-address`，见 3.2）
- 不传 `--home` / `--data` / `--config`，不设 `STHOMEDIR` / `STDATADIR` 等环境变量
- API Key 从 home 的 `config.xml` 读取

**可以变**
- 谁去 spawn 这个进程（SyncTrayzor → FtpMount）
- 托盘图标、通知、界面语言、日志窗口、窗口尺寸
- 开机自启的实现方式（SyncTrayzor 的登录启动 → Tauri autostart 插件）

**绝对禁止**
- 新建/清空 home 目录，或让 FtpMount 自带一份空白配置
- 删除或重建 `index-v2`，使用 `--reset-database` / `--reset-deltas`
- 替换/重新生成 `cert.pem` / `key.pem`
- 用不同版本的 exe 直接打开现有 home
- 在 Rust 侧另存一份文件夹/设备配置（双份配置必然漂移）
- 把设备 ID、API Key、真实文件夹路径写进源码、日志样例、配置模板或安装包

**验收判据**（切换后逐条核对，全部满足才算接管成功）
1. `GET /rest/system/status` 的 `myID` 与切换前一致
2. `GET /rest/config/folders|devices` 数量与 ID 与切换前一致
3. 每个文件夹 `GET /rest/db/status` 为 `idle`，全局/本地文件数不变，无待传字节
4. 远程设备状态仍为「最新」，没有出现「未连接 / 等待配对」
5. 日志里没有出现全量扫描（full scan / hashing）记录

## 五、控制接口事实

- 接口挂在 **Syncthing 的 GUI 端口**（官方默认 8384，实际值以 `config.xml` 的 `<address>` 为准）。
  配置为 `enabled=true, tls=false`，**纯 HTTP，无需处理自签证书**。
- 鉴权：`X-API-Key: <key>` 或 `Authorization: Bearer <key>`；不带 key 访问受保护端点返回 **403**；
  唯一免鉴权的是 `GET /rest/noauth/health`（实测 200 / `{"status":"OK"}`）。
- 事件流是 **HTTP 长轮询**，不是 WebSocket：`GET /rest/events?since=<id>&timeout=<秒>`，
  无新事件时阻塞，默认 60 秒返回空数组；`limit=1` 可用于断线后追上最新 ID。
- 配置端点用 `/rest/config/...`（v1.12+，v2 沿用）：
  `GET/PUT /rest/config`、`GET/POST/PUT /rest/config/folders|devices`、
  `GET/PUT/PATCH/DELETE /rest/config/folders/<id>`、`/rest/config/options|gui`、
  `GET /rest/config/restart-required`。旧的 `/rest/system/config` **已废弃**。
- 官方声明 "The interface is subject to change" —— 需要按版本适配。

### 客户端会用到的端点（约 12 个）

| 用途 | 端点 |
|---|---|
| 探活（无 key） | `GET /rest/noauth/health` |
| 本机 ID / 版本 / 运行时间 / home 路径 | `/rest/system/status`、`/rest/system/version`、`/rest/system/paths` |
| 设备连接与速率 | `/rest/system/connections`、`/rest/stats/device` |
| **配置读取（接管核心）** | `/rest/config/folders`、`/rest/config/devices`、`/rest/config/options` |
| 文件夹清单与同步进度 | `/rest/db/status?folder=<id>`、`/rest/db/completion` |
| 实时变化 | `GET /rest/events?since=...` |
| 常用控制 | `POST /rest/db/scan`、`/rest/system/pause`、`/rest/system/resume`、`/rest/system/error/clear` |
| 日志 | `GET /rest/system/log` |
| 配置写入（需用户确认） | `PUT/PATCH /rest/config/...` |

## 六、与 FtpMount 的集成设计

现有可复用的部分（都不用改架构）：

- `desktop/src-tauri/` 已启用 `tray-icon`、`tauri-plugin-autostart`、`tauri-plugin-single-instance`
- `main.rs`：托盘菜单 `build_tray_menu()`、`goto-tab` 事件分发
- `cmds.rs`：`#[tauri::command]` + `generate_handler![]` 注册模式
- `dpapi.rs`：`encrypt()` / `decrypt()` —— **用来存 API Key，不落明文**
- `desktop/src/index.html`：已有「连接 / 防锁屏 / 日志 / 关于」四个页签，加第五个即可
- 前端是纯静态 HTML/JS + `withGlobalTauri`，无 Node 构建步骤

| 文件 | 改动 |
|---|---|
| `desktop/src-tauri/src/syncthing.rs`（新增） | REST 薄封装、`config.xml` 解析、home 与 exe 定位、启动参数拼接、状态结构体 |
| `desktop/src-tauri/src/cmds.rs` | 5–8 个命令：`sync_status` / `sync_folders` / `sync_devices` / `sync_pause` / `sync_resume` / `sync_rescan` / `sync_save_*` / `open_web_gui` |
| `desktop/src-tauri/src/main.rs` | `mod syncthing;`、注册命令、托盘加同步状态 |
| `desktop/src-tauri/Cargo.toml` | 加 HTTP 客户端 + XML 解析 |
| `desktop/src/index.html` / `app.js` / `style.css` | 新增「同步」页签与面板 |
| `CHANGELOG.md` + 三处版本号 | 按项目发版规矩：末位 +1（0.1.12 → 0.1.13）并同步两个 `Cargo.toml` 与 `tauri.conf.json` |

**根 crate `src/`（挂载核心）一行都不用动**，所以现有挂载与防锁屏功能零风险。

### 依赖选型

- 推荐 **`ureq`**（同步 HTTP、无 tokio、默认不读系统代理）。现有代码全是同步风格，
  长轮询单独起一个 `std::thread` 即可。
- 若用 `reqwest`，**必须显式 `.no_proxy()`**：本机存在 `HTTP_PROXY`/`HTTPS_PROXY`，
  实测连回环地址时 curl 也会先发 `CONNECT` 隧道请求。
- XML 解析：`quick-xml` 或极简正则，只读 home 下 `config.xml` 的 `<apikey>` 与 `<address>`。
- 前端 JS 不持有 API Key，所有请求走 Rust 侧 `invoke`；Key 用 `dpapi::encrypt()` 保存
  （与现有 FTP 密码同一套机制），不写日志、不进仓库、不进安装包默认值。

### 进程托管（P3）的正确形态

```text
读取：home = %LOCALAPPDATA%\Syncthing（不显式指定，保持默认）
      exe  = 事先另存的、与当前同版本的 syncthing.exe
      key  = home/config.xml 的 <apikey>
启动：<exe> --no-browser --no-restart --gui-address=<原值>
连接：/rest/noauth/health 轮询等待就绪 → 带 X-API-Key 调 REST
退出：FtpMount 退出时不要杀 daemon（保持独立，避免误伤同步）
```

FtpMount 与 SyncTrayzor 不可同时托管 daemon：两者会抢同一 GUI 端口。
切换窗口期必须明确「谁负责启动」，见第九节路线。

## 七、功能覆盖（按用户要求修正）

「配置接过来」有两层含义，都要满足：

1. **配置不丢**（第四节）——靠 home/证书/索引/exe 原样保留，与界面无关。
2. **配置在新 GUI 里可见、可管**——必须实现：
   - 文件夹列表与详情（路径、类型、共享设备、忽略模式、版本控制状态）
   - 设备列表与详情、待接受的设备/文件夹
   - 常用编辑：暂停/恢复、重新扫描、忽略规则、文件夹类型、共享关系
   - 这些全部通过 `/rest/config/...` 读写同一份配置，不另建存储

**高级项不自己重写**：全局选项、LDAP、发现服务器、复杂版本策略等，
面板里放「打开完整设置」入口，用系统浏览器或内嵌 WebView 打开官方 Web GUI。
理由不是「做不到」，而是复刻成本高、必然落后上游版本；有这个入口功能就一个不丢。

**明确不做**：用 Rust 重写 BEP 同步协议。crates.io 上有 `rbep` 之类的实现，
但那是另一个量级的工程，且与「替代 GUI」无关。

## 八、风险（按严重程度重排）

1. **home / 设备身份 / 索引被改动** → 直接导致重新扫描、重新配对，即用户最担心的后果。
   护栏见第四节「绝对禁止」与验收判据。
2. **`syncthing.exe` 随 SyncTrayzor 卸载而消失**，或误用旧版 exe 打开 `index-v2` →
   接管前先另存并核对版本。
3. **误加 `--gui-address`**：该参数会被写回 `config.xml`，构成对生产配置的写入；
   地址本来就在配置里，不要画蛇添足（见 3.2）。
4. **谁来启动 daemon**：并存期让 SyncTrayzor 继续启动；切换期必须停掉它再交给 FtpMount，
   不能两边都 spawn。FtpMount 是 perMachine 桌面应用，spawn 还要处理工作目录与退出语义。
5. **接口版本适配**：本机 v2.1.7，官方声明接口可变。第三方 crate `syncthing` 0.6.0
   （2025-09，配套 `syncthing-async` / `syncthing-types`）是唯一稳定版，但覆盖度与面向的
   API 版本未验证且绑定其异步运行时；`syncthing-rs` 仍是 alpha。
   **结论：自己薄封装约 12 个端点，不引第三方客户端 crate。**
6. **代理**：见第六节，`ureq` 或显式 `.no_proxy()`。
7. **写操作的生产风险**：`PUT/PATCH /rest/config` 立即生效，误操作会直接改变同步关系。
   开发期只做 GET；写操作逐条、只针对用户明确指定的对象，并先 dry-run。
8. **对外程序不许带私人痕迹**（项目硬规则）：设备 ID、设备名、真实文件夹路径、
   API Key、本机用户名一律不进源码、默认值、日志样例、配置模板和安装包。默认值留空。
9. **长轮询生命周期**：隐藏到托盘时事件线程不能阻塞退出；退出时要能中断阻塞请求
   （`timeout` 设 5–10 秒比默认 60 秒稳妥）。

## 九、分期路线

| 阶段 | 内容 | 规模 | 与 SyncTrayzor 的关系 |
|---|---|---|---|
| P0 | **只读验收配置接管**：定位 home/exe/参数，读 key，调 REST 打印文件夹/设备/同步状态摘要；确认设备 ID 与文件数基线 | ~150–250 行，放 `_work/`，零生产改动 | **并存**（本次评估已跑通一遍） |
| P1 | 独立 Rust 客户端：完整配置视图 + 常用编辑 + 事件长轮询 + 托盘 + Key 加密存储 | ~500–800 行 | **并存**，替代日常查看与操作 |
| P2 | 集成进 FtpMount：新增「同步」页签 + Tauri 命令 + 托盘状态 | 改 6 个文件，不动 `src/` | 仍并存 |
| P3 | **切换**：备份 home → 另存同版本 exe → 停止并退出 SyncTrayzor → FtpMount 用原参数 spawn → 按第四节 5 条判据验收 → 全部通过后才卸载 SyncTrayzor | 最难，风险最高 | **替换** |

P3 的**验收先于卸载**：只要第 1 条（设备 ID 一致）或第 3 条（db 状态）不对，
立刻退回让 SyncTrayzor 重新启动，配置从未被改动，可随时回退。

## 十、待拍板

用户已明确：目标是**替换现在的 GUI，且配置必须接过来、不能重新同步**。
仍需确认两点：

1. 新 GUI 是否要做**配置写入**（添加文件夹/设备、改共享关系、改忽略规则），
   还是只读 + 暂停/重扫这类运行时控制就够？
2. daemon 未运行时，是否允许 FtpMount 直接拉起（即执行 P3 的进程托管）？
   还是先只连一个已由别处启动的 daemon？

（若只是嫌 SyncTrayzor 停维护，官方推荐 `GermanCoding/SyncTrayzor` v2；
但用户目标是集成进 FtpMount，自写仍是合理选择。）
