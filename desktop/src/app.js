/* FtpMount 前端逻辑（无框架，保持内存占用最小） */

const invoke = window.__TAURI__.core.invoke;
const listen = window.__TAURI__.event.listen;

const $ = (id) => document.getElementById(id);

let store = { sites: [], settings: {}, active_id: "" };
let currentId = "";
let busy = false;
let mounted = false;
let starting = false; // 后端正在挂载（开机自动挂载时会短暂为 true）
let suppressSettings = false;

/* ---------------- 工具 ---------------- */

function setHint(text, kind = "") {
  const el = $("hint");
  el.textContent = text || "";
  el.className = "hint" + (kind ? " " + kind : "");
}

function setBusy(on, text) {
  busy = on;
  document.body.style.cursor = on ? "progress" : "";
  if (on && text) setHint(text, "busy");
  refreshButtons();
}

function refreshButtons() {
  // 注意：新建站点在保存前 currentId 是空串，不能只用它判断可用性，
  // 否则「保存」按钮也被禁用，就永远保存不了、永远挂载不了。
  const hasSite = !!currentId;
  const canAct = !busy && (hasSite || $("f-host").value.trim() !== "");
  // 正在挂载（多半是开机自动挂载）时把按钮都按住：这时候点「挂载」只会撞车，
  // 以前就是在这里撞出「对象名已存在」的假失败。
  $("btn-mount").disabled = !canAct || mounted || starting;
  $("btn-unmount").disabled = busy || starting || !mounted;
  $("btn-open").disabled = !mounted;
  $("btn-rw").disabled = busy || !mounted;
  $("btn-check").disabled = !canAct || starting;
  $("btn-save").disabled = !canAct || starting;
  $("btn-del").disabled = busy || starting || !hasSite;
}

/* ---------------- 表单 ---------------- */

function driveOptions() {
  const sel = $("f-drive");
  const keep = sel.value;
  sel.innerHTML = "";
  for (let c = 67; c <= 90; c++) {
    const d = String.fromCharCode(c) + ":";
    const o = document.createElement("option");
    o.value = d;
    o.textContent = d;
    sel.appendChild(o);
  }
  sel.value = keep || "R:";
}

function fillForm(site) {
  $("f-name").value = site.name || "";
  $("f-host").value = site.host || "";
  $("f-port").value = site.port || 21;
  $("f-user").value = site.user || "";
  $("f-pass").value = site.pass || "";
  $("f-path").value = site.remote_path || "/";
  $("f-drive").value = site.drive || "R:";
  $("f-tz").value = site.list_tz_offset ?? 480;
  $("f-tls").checked = !!site.tls;
  updateTarget();
}

/** 卡片底部的「挂载到」预览：一眼看清最终连的是哪个地址、挂到哪个盘 */
function updateTarget() {
  const host = $("f-host").value.trim();
  const port = $("f-port").value.trim() || "21";
  let path = $("f-path").value.trim() || "/";
  if (!path.startsWith("/")) path = "/" + path;
  const drive = $("f-drive").value || "R:";
  const scheme = $("f-tls").checked ? "ftps" : "ftp";
  const el = $("target-url");
  if (host) {
    el.textContent = `${scheme}://${host}:${port}${path}  →  ${drive}`;
    el.classList.remove("dim");
  } else {
    el.textContent = "填好服务器地址后显示";
    el.classList.add("dim");
  }
}

function readForm() {
  return {
    id: currentId,
    name: $("f-name").value.trim() || $("f-host").value.trim() || "未命名",
    host: $("f-host").value.trim(),
    port: parseInt($("f-port").value, 10) || 21,
    user: $("f-user").value,
    pass: $("f-pass").value,
    remote_path: $("f-path").value.trim() || "/",
    drive: $("f-drive").value || "R:",
    tls: $("f-tls").checked,
    list_tz_offset: parseInt($("f-tz").value, 10) || 0,
  };
}

/* ---------------- 站点列表 ---------------- */

function renderSites() {
  const sel = $("site-select");
  sel.innerHTML = "";
  if (store.sites.length === 0) {
    const o = document.createElement("option");
    o.value = "";
    o.textContent = "（还没有站点）";
    sel.appendChild(o);
    currentId = "";
    return;
  }
  for (const s of store.sites) {
    const o = document.createElement("option");
    o.value = s.id;
    o.textContent = s.name + (s.host ? `  ·  ${s.host}` : "");
    sel.appendChild(o);
  }
  if (!store.sites.some((s) => s.id === store.active_id)) {
    store.active_id = store.sites[0].id;
  }
  sel.value = store.active_id;
  selectSite(store.active_id, false);
}

function selectSite(id, persist = true) {
  const site = store.sites.find((s) => s.id === id);
  if (!site) {
    currentId = "";
    fillForm({});
    refreshButtons();
    return;
  }
  currentId = id;
  store.active_id = id;
  fillForm(site);
  $("site-select").value = id;
  if (persist) invoke("set_active", { id }).catch(() => {});
  refreshButtons();
}

/* ---------------- 设置 ---------------- */

function applySettings() {
  suppressSettings = true;
  const s = store.settings || {};
  $("opt-autostart").checked = !!s.autostart;
  $("opt-minclose").checked = s.minimize_on_close !== false;
  $("opt-verbose").checked = !!s.verbose;
  document.documentElement.dataset.theme = s.theme === "light" ? "light" : "dark";
  suppressSettings = false;
}

async function pushSettings() {
  if (suppressSettings) return;
  const settings = {
    autostart: $("opt-autostart").checked,
    minimize_on_close: $("opt-minclose").checked,
    verbose: $("opt-verbose").checked,
    theme: document.documentElement.dataset.theme || "dark",
    auto_mount_id: (store.settings && store.settings.auto_mount_id) || "",
  };
  try {
    store = await invoke("set_settings", { settings });
    applySettings();
  } catch (e) {
    setHint(String(e), "err");
  }
}

/* ---------------- 状态 ---------------- */

function renderStatus(st) {
  const card = $("status-card");
  card.className = "status";
  mounted = !!st.mounted;
  starting = !!st.starting;

  if (busy) {
    card.classList.add("busy");
    $("st-title").textContent = "处理中…";
    $("st-sub").textContent = "请稍候";
  } else if (starting) {
    // 开机自启：后端正在静默挂载。这里必须显式告诉用户，否则界面看着像"没挂上"，
    // 用户就会去点「挂载」，两个挂载请求撞在一起报「对象名已存在」。
    card.classList.add("busy");
    $("st-title").textContent = "正在挂载 " + (st.drive || "");
    $("st-sub").textContent = st.site || "请稍候";
  } else if (st.mounted) {
    card.classList.add("on");
    $("st-title").textContent = st.drive + " 已挂载";
    $("st-sub").textContent = st.site || "";
  } else if (st.error) {
    card.classList.add("err");
    $("st-title").textContent = "挂载失败";
    $("st-sub").textContent = st.error.slice(0, 60);
  } else {
    $("st-title").textContent = "未挂载";
    $("st-sub").textContent = currentId ? "点「挂载」开始" : "先选一个站点";
  }
  refreshButtons();
}

async function pollStatus() {
  try {
    renderStatus(await invoke("status"));
  } catch (_) {}
}

/* ---------------- 日志 ---------------- */

const logsEl = () => $("logs");
let logCount = 0;
let logPending = [];
let logScheduled = false;

// 攒一帧再渲染：以前每来一行就 createElement + 读 scrollHeight（强制同步布局），
// 拷贝时一次涌进几十行，WebView 主线程会被拖住几百毫秒甚至更久，
// 而主线程一忙，Rust 那边的日志推送就会反压到文件系统线程上。
function appendLog(line) {
  logPending.push(line);
  if (!logScheduled) {
    logScheduled = true;
    requestAnimationFrame(flushLogs);
  }
}

function flushLogs() {
  logScheduled = false;
  const box = logsEl();
  const batch = logPending;
  logPending = [];
  if (!box || !batch.length) return;

  const nearBottom = box.scrollHeight - box.scrollTop - box.clientHeight < 60;
  const frag = document.createDocumentFragment();
  for (const line of batch) {
    const row = document.createElement("div");
    row.className = "log-line " + (line.level || "INFO");
    const t = document.createElement("span");
    t.className = "t";
    t.textContent = line.ts || "";
    const lv = document.createElement("span");
    lv.className = "lv";
    lv.textContent = (line.level || "INFO").toLowerCase();
    const m = document.createElement("span");
    m.className = "m";
    m.textContent = line.text || "";
    row.append(t, lv, m);
    frag.appendChild(row);
    logCount++;
  }
  box.appendChild(frag);
  while (logCount > 2000 && box.firstChild) {
    box.removeChild(box.firstChild);
    logCount--;
  }
  if (nearBottom) box.scrollTop = box.scrollHeight;
}

/* ---------------- 动作 ---------------- */

async function doCheck() {
  if (!$("f-host").value.trim()) {
    setHint("先填服务器地址", "err");
    return;
  }
  setBusy(true, "正在检测连接…");
  try {
    const r = await invoke("check_conn", { site: readForm() });
    if (r.ok) {
      const w = r.writable ? "可写" : "只读";
      setHint(
        `连接成功 · ${r.ms} ms · ${r.items} 项 · ${w} · ${r.mlsd ? "支持 MLSD" : "仅 LIST"}`,
        "ok"
      );
    } else {
      setHint(r.msg, "err");
    }
  } catch (e) {
    setHint(String(e), "err");
  } finally {
    setBusy(false);
    pollStatus();
  }
}

async function saveSite(quiet = false) {
  const site = readForm();
  if (!site.host) {
    if (!quiet) setHint("服务器地址不能为空", "err");
    return false;
  }
  store = await invoke("save_site", { site });
  currentId = store.active_id;
  renderSites();
  if (!quiet) setHint("已保存", "ok");
  return true;
}

async function doSave() {
  setBusy(true);
  try {
    await saveSite(false);
  } catch (e) {
    setHint(String(e), "err");
  } finally {
    setBusy(false);
  }
}

async function doMount() {
  if (mounted || starting) return;
  if (!$("f-host").value.trim()) {
    setHint("先填服务器地址", "err");
    return;
  }
  setBusy(true, "正在挂载…");
  try {
    if (!(await saveSite(true))) return;
    const site = readForm();
    await invoke("mount", { site, drive: site.drive });
    setHint(`${site.drive} 挂载成功`, "ok");
  } catch (e) {
    setHint(String(e), "err");
  } finally {
    setBusy(false);
    pollStatus();
  }
}

async function doUnmount() {
  if (!mounted) return;
  setBusy(true, "正在卸载…");
  try {
    await invoke("unmount");
    setHint("已卸载", "ok");
  } catch (e) {
    setHint(String(e), "err");
  } finally {
    setBusy(false);
    pollStatus();
  }
}

async function doOpen() {
  const drive = $("f-drive").value || "R:";
  try {
    await invoke("open_drive", { drive });
  } catch (e) {
    setHint(String(e), "err");
  }
}

async function doRw() {
  setBusy(true, "正在测试读写…");
  try {
    const r = await invoke("test_rw");
    setHint(r.msg, r.ok ? "ok" : "err");
  } catch (e) {
    setHint(String(e), "err");
  } finally {
    setBusy(false);
  }
}

/* ---------------- 防锁屏 ---------------- */

let kaConfig = { force: false, entries: [] };
let kaStatus = { active: false, forced: false, matched: [] };
let kaRunning = [];
let kaListSig = "";

function kaHint(text, kind = "") {
  const el = $("ka-hint");
  el.textContent = text || "";
  el.className = "hint" + (kind ? " " + kind : "");
}

function renderKaBadge() {
  const b = $("ka-badge");
  if (kaStatus.forced) {
    b.className = "ka-badge force";
    b.textContent = "强制常亮中";
    b.title = "不看列表，一直保持屏幕常亮";
  } else if (kaStatus.active) {
    const who = (kaStatus.matched || []).join(", ");
    b.className = "ka-badge on";
    b.textContent = "常亮中 · " + who;
    b.title = "正在由 " + who + " 触发";
  } else {
    b.className = "ka-badge";
    b.textContent = "未触发";
    b.title = "列表里的程序都没有在运行";
  }
  // 常亮时标题栏的 FtpMount 字样变红，一眼能看出当前是不是在防锁屏
  document.body.classList.toggle("ka-on", !!kaStatus.active);
}

function renderKaList() {
  const box = $("ka-list");
  box.innerHTML = "";

  if (!kaConfig.entries.length) {
    const d = document.createElement("div");
    d.className = "ka-empty";
    d.textContent =
      "还没有添加程序。用上面的下拉框从正在运行的程序里挑，或者手动输入进程名。";
    box.appendChild(d);
    return;
  }

  const running = new Set(kaRunning.map((p) => p.name.toLowerCase()));

  kaConfig.entries.forEach((e, i) => {
    const row = document.createElement("div");
    row.className = "ka-item";

    const cb = document.createElement("input");
    cb.type = "checkbox";
    cb.checked = e.enabled !== false;
    cb.title = "启用 / 停用这一条";
    cb.addEventListener("change", () => {
      kaConfig.entries[i].enabled = cb.checked;
      saveKa();
    });

    const nm = document.createElement("span");
    nm.className = "nm";
    nm.textContent = e.name;

    const st = document.createElement("span");
    const isRun = running.has((e.name || "").toLowerCase());
    st.className = "st" + (isRun ? " run" : "");
    st.textContent = isRun ? "运行中" : "未运行";

    const del = document.createElement("button");
    del.className = "btn ghost danger";
    del.textContent = "删除";
    del.addEventListener("click", () => {
      kaConfig.entries.splice(i, 1);
      saveKa();
    });

    row.append(cb, nm, st, del);
    box.appendChild(row);
  });
}

/** 列表只在「配置或运行中进程集合」变了才重建，否则每 2 秒刷新会打断勾选操作 */
function renderKaListIfChanged() {
  const sig =
    JSON.stringify(kaConfig) + "|" + kaRunning.map((p) => p.name.toLowerCase()).join(",");
  if (sig === kaListSig) return;
  kaListSig = sig;
  renderKaList();
}

function renderKa() {
  $("ka-force").checked = !!kaConfig.force;
  renderKaBadge();
  renderKaListIfChanged();
}

async function loadKa() {
  try {
    const v = await invoke("keepawake_view");
    kaConfig = v.config || { force: false, entries: [] };
    kaStatus = v.status || { active: false, forced: false, matched: [] };
    const p = $("ka-path");
    p.textContent = v.path || "—";
    p.title = v.beside_exe
      ? "就在 exe 旁边，可以直接改"
      : "exe 所在目录不可写（多半装在 Program Files），已回落到 %APPDATA%";
    renderKa();
  } catch (e) {
    kaHint(String(e), "err");
  }
}

async function refreshKaRunning() {
  try {
    kaRunning = await invoke("keepawake_processes");
  } catch (_) {
    kaRunning = [];
  }
  const sel = $("ka-pick");
  const keep = sel.value;
  sel.innerHTML = "";
  if (!kaRunning.length) {
    const o = document.createElement("option");
    o.value = "";
    o.textContent = "（没读到正在运行的程序）";
    sel.appendChild(o);
  }
  for (const p of kaRunning) {
    const o = document.createElement("option");
    o.value = p.name;
    o.textContent = p.title ? `${p.name}  —  ${p.title}` : p.name;
    sel.appendChild(o);
  }
  if (keep && kaRunning.some((p) => p.name === keep)) sel.value = keep;
  renderKaListIfChanged();
}

async function saveKa() {
  try {
    const v = await invoke("keepawake_save", { cfg: kaConfig });
    kaConfig = v.config || kaConfig;
    kaStatus = v.status || kaStatus;
    kaListSig = ""; // 强制重建，让勾选状态跟着落盘结果走
    renderKa();
    kaHint("已保存", "ok");
  } catch (e) {
    kaHint(String(e), "err");
  }
}

function addKaEntry(raw) {
  let name = (raw || "").trim();
  if (!name) {
    kaHint("先输入或选择一个进程名", "err");
    return;
  }
  if (!name.toLowerCase().endsWith(".exe")) name += ".exe";
  if (kaConfig.entries.some((e) => e.name.toLowerCase() === name.toLowerCase())) {
    kaHint(`${name} 已经在列表里了`, "err");
    return;
  }
  kaConfig.entries.push({ name, enabled: true });
  $("ka-name").value = "";
  saveKa();
}

/* ---------------- 初始化 ---------------- */

function activateTab(name) {
  document.querySelectorAll(".nav-item").forEach((b) => {
    b.classList.toggle("active", b.dataset.tab === name);
  });
  document.querySelectorAll(".tab").forEach((t) => {
    t.classList.toggle("active", t.id === "tab-" + name);
  });
  // 切到防锁屏页签时刷一次：进程列表是实时快照，久不看就会过期
  if (name === "awake") {
    loadKa();
    refreshKaRunning();
  }
}

function bindTabs() {
  document.querySelectorAll(".nav-item").forEach((btn) => {
    btn.addEventListener("click", () => activateTab(btn.dataset.tab));
  });
}

function bindWindowControls() {
  $("w-min").addEventListener("click", () => invoke("win_minimize"));
  $("w-max").addEventListener("click", () => invoke("win_toggle_max"));
  $("w-close").addEventListener("click", () => invoke("win_close"));
}

async function boot() {
  bindTabs();
  bindWindowControls();

  invoke("js_probe", {
    w: window.innerWidth,
    h: window.innerHeight,
    dpr: window.devicePixelRatio,
  }).catch(() => {});

  const info = await invoke("app_info");
  $("brand-ver").textContent = "v" + info.version;
  $("about-ver").textContent = "v" + info.version;
  $("about-build").textContent = info.build_time;
  $("about-meta").innerHTML = [
    `<div><b>可执行文件</b> ${info.exe}</div>`,
    `<div><b>WinFsp</b> ${info.winfsp ? "已安装" : "未安装（挂载前请先安装 WinFsp 2.x）"}</div>`,
    `<div><b>系统</b> ${info.os}</div>`,
    `<div><b>配置</b> %APPDATA%\\FtpMount\\config.json</div>`,
  ].join("");

  store = await invoke("load_store");
  driveOptions();
  applySettings();

  if (store.sites.length === 0) {
    // 首次运行：给一个空壳站点，用户填完直接保存
    store.sites = [
      {
        id: "",
        name: "新站点",
        host: "",
        port: 21,
        user: "",
        pass: "",
        remote_path: "/",
        drive: "R:",
        tls: false,
        list_tz_offset: 480,
      },
    ];
    store.active_id = "";
    currentId = "";
    fillForm(store.sites[0]);
    $("site-select").innerHTML = '<option value="">（未保存）</option>';
  } else {
    renderSites();
  }

  // 站点切换
  $("site-select").addEventListener("change", (e) => {
    if (e.target.value) selectSite(e.target.value);
  });

  $("btn-new").addEventListener("click", () => {
    const site = {
      id: "",
      name: "新站点",
      host: "",
      port: 21,
      user: "",
      pass: "",
      remote_path: "/",
      drive: $("f-drive").value || "R:",
      tls: false,
      list_tz_offset: 480,
    };
    currentId = "";
    fillForm(site);
    setHint("填写信息后点「保存」", "");
    refreshButtons();
  });

  $("btn-del").addEventListener("click", async () => {
    if (!currentId) return;
    if (!confirm("确定删除这个站点？")) return;
    store = await invoke("delete_site", { id: currentId });
    renderSites();
    setHint("已删除", "ok");
  });

  $("btn-check").addEventListener("click", doCheck);
  $("btn-save").addEventListener("click", doSave);
  $("btn-mount").addEventListener("click", doMount);
  $("btn-unmount").addEventListener("click", doUnmount);
  $("btn-open").addEventListener("click", doOpen);
  $("btn-rw").addEventListener("click", doRw);

  // 新建站点时靠「服务器」是否填了来决定按钮可用性
  ["f-host", "f-name"].forEach((id) => $(id).addEventListener("input", refreshButtons));
  // 「挂载到」预览跟着输入实时更新
  ["f-host", "f-port", "f-path"].forEach((id) => $(id).addEventListener("input", updateTarget));
  ["f-tls", "f-drive"].forEach((id) => $(id).addEventListener("change", updateTarget));

  ["opt-autostart", "opt-minclose", "opt-verbose"].forEach((id) =>
    $(id).addEventListener("change", pushSettings)
  );

  $("btn-logclear").addEventListener("click", async () => {
    await invoke("clear_logs");
    logPending = [];
    logsEl().innerHTML = "";
    logCount = 0;
  });
  $("btn-logdir").addEventListener("click", () => invoke("open_log_dir"));

  // 防锁屏
  $("ka-force").addEventListener("change", () => {
    kaConfig.force = $("ka-force").checked;
    saveKa();
  });
  $("ka-add").addEventListener("click", () => addKaEntry($("ka-name").value));
  $("ka-name").addEventListener("keydown", (e) => {
    if (e.key === "Enter") addKaEntry($("ka-name").value);
  });
  $("ka-pick-add").addEventListener("click", () => addKaEntry($("ka-pick").value));
  $("ka-refresh").addEventListener("click", refreshKaRunning);

  await loadKa();
  refreshKaRunning();

  // 只在防锁屏页签可见时轮询，不打扰别的页面。
  // 状态每 2 秒一次（便宜），进程快照每 10 秒一次（要遍历全部进程 + 窗口）。
  let kaTick = 0;
  setInterval(() => {
    if (!$("tab-awake").classList.contains("active")) return;
    kaTick++;
    invoke("keepawake_view")
      .then((v) => {
        kaStatus = v.status || kaStatus;
        renderKaBadge();
      })
      .catch(() => {});
    if (kaTick % 5 === 0) refreshKaRunning();
  }, 2000);

  // 历史日志 + 实时日志
  try {
    (await invoke("logs")).forEach(appendLog);
  } catch (_) {}
  await listen("log", (e) => appendLog(e.payload));
  await listen("tray-mount", () => doMount());
  // 点托盘菜单里的状态行 → 打开窗口并直接跳到「防锁屏」页签
  await listen("goto-tab", (e) => activateTab(e.payload));
  // 防锁屏状态一变就推过来：标题栏的 FtpMount 是否变红不依赖当前停在哪个页签
  await listen("keepawake", (e) => {
    kaStatus = e.payload || kaStatus;
    renderKaBadge();
    renderKaListIfChanged();
  });

  await pollStatus();
  setInterval(pollStatus, 1500);
}

window.addEventListener("DOMContentLoaded", boot);
