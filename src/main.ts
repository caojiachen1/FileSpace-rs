// FileSpace 前端：Win11 资源管理器 UI
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { PhysicalPosition } from "@tauri-apps/api/dpi";
import { listen } from "@tauri-apps/api/event";

/* ===================== 类型 ===================== */
interface ShellEntry {
  name: string;
  full_name: string;
  parse_path: string;
  fs_path: string | null;
  is_folder: boolean;
  sort_as_folder: boolean;
  is_hidden: boolean;
  size: number | null;
  size_text: string;
  date_modified: number;
  date_text: string;
  date_created: number;
  date_created_text: string;
  type_text: string;
  ext: string;
  drive_total: number | null;
  drive_free: number | null;
  drive_text: string;
  pinned: boolean;
}
interface Crumb { name: string; parse_path: string; }
interface FolderListing {
  folder_name: string;
  parse_path: string;
  breadcrumb: Crumb[];
  entries: ShellEntry[];
}
interface SidebarData {
  quick_access: ShellEntry[];
  this_pc: ShellEntry[];
  drives: ShellEntry[];
  network: ShellEntry | null;
  linux: ShellEntry | null;
}
interface MenuResult { action: string; verb: string; }

const THIS_PC = "::{20D04FE0-3AEA-1069-A2D8-08002B30309D}";
const LINUX_WSL = "::{B2B4A4D1-2754-4140-A2EB-9A76D9D7CDC6}";

/* ===================== 视图与显示设置 ===================== */
type ViewMode = "xl-icons" | "l-icons" | "m-icons" | "s-icons" | "list" | "details" | "tiles" | "content";

// 各视图的图标尺寸与是否用缩略图（与资源管理器一致：中/大/超大用缩略图）
const VIEW_CFG: Record<ViewMode, { icon: number; thumb: boolean }> = {
  "xl-icons": { icon: 256, thumb: true },
  "l-icons": { icon: 96, thumb: true },
  "m-icons": { icon: 48, thumb: true },
  "s-icons": { icon: 16, thumb: false },
  list: { icon: 16, thumb: false },
  details: { icon: 32, thumb: false },
  tiles: { icon: 48, thumb: false },
  content: { icon: 32, thumb: false },
};

// 显示设置（全局，对应"显示"子菜单）
const settings = {
  navPane: true,
  compact: false,
  checkboxes: false,
  showExt: true,
  showHidden: true,
  detailsPane: false,
  previewPane: false,
};

/* ===================== 状态 ===================== */
type SortKey = "name" | "date" | "created" | "type" | "size";

interface Tab {
  id: number;
  history: string[];
  historyIndex: number;
  listing: FolderListing | null;
  selection: Set<string>;
  anchorIndex: number;
  sortKey: SortKey;
  sortAsc: boolean;
  filter: string;
  view: ViewMode;
}
let tabSeq = 0;
const tabs: Tab[] = [];
let activeTabIdx = 0;
let sidebar: SidebarData | null = null;
let cutPaths = new Set<string>();
const iconCache = new Map<string, string>();

const $ = <T extends HTMLElement = HTMLElement>(id: string) => document.getElementById(id) as T;
const appWindow = getCurrentWindow();

function newTab(path: string): Tab {
  return {
    id: ++tabSeq,
    history: [path],
    historyIndex: 0,
    listing: null,
    selection: new Set(),
    anchorIndex: -1,
    sortKey: "name",
    sortAsc: true,
    filter: "",
    view: "details",
  };
}
const activeTab = () => tabs[activeTabIdx];

/* ===================== 图标 ===================== */
const getIcon = (path: string, size: number) => iconCache.get(`${size}|${path}`);

async function loadIcons(entries: ShellEntry[], targets: () => Map<string, HTMLImageElement>, size = 32) {
  const missing = entries.filter((e) => !iconCache.has(`${size}|${e.parse_path}`));
  const CHUNK = 40;
  for (let i = 0; i < missing.length; i += CHUNK) {
    const chunk = missing.slice(i, i + CHUNK);
    const icons = await invoke<(string | null)[]>("get_icons", {
      items: chunk.map((e) => ({ path: e.parse_path, is_folder: e.sort_as_folder, ext: e.ext })),
      size,
    });
    chunk.forEach((e, j) => {
      const icon = icons[j];
      if (icon) iconCache.set(`${size}|${e.parse_path}`, icon);
    });
    // 局部更新已渲染的 img
    const map = targets();
    chunk.forEach((e) => {
      const img = map.get(e.parse_path);
      const icon = getIcon(e.parse_path, size);
      if (img && icon) img.src = icon;
    });
  }
}

/* ===================== 导航 ===================== */
// 乐观导航：点击后立即高亮目标并显示加载态，枚举完成后再填充（慢速位置如网络不阻塞反馈）
let navToken = 0;
let pendingPath: string | null = null;
// 目录列表缓存：回访/新建标签页秒开，后台刷新最新数据
const listingCache = new Map<string, FolderListing>();
const LISTING_CACHE_MAX = 30;

function cacheListing(key: string, listing: FolderListing) {
  listingCache.delete(key);
  listingCache.set(key, listing);
  listingCache.delete(listing.parse_path);
  listingCache.set(listing.parse_path, listing);
  while (listingCache.size > LISTING_CACHE_MAX) {
    const oldest = listingCache.keys().next().value as string;
    listingCache.delete(oldest);
  }
}

async function navigate(path: string, opts: { push?: boolean; selectPath?: string } = { push: true }) {
  const tab = activeTab();
  const token = ++navToken;
  pendingPath = path;
  const cached = listingCache.get(path);
  let pushed = false;
  // 并行查询 ShellBag 中保存的视图模式（与资源管理器共享，无记录时保持当前视图）
  const viewPromise = invoke<string | null>("get_view_mode", { path }).catch(() => null);

  // 向上返回时选中来源文件夹并滚动到可见（与资源管理器一致）
  const applySelect = () => {
    const sp = opts.selectPath;
    if (!sp) return;
    const list = sortedEntries(tab);
    const idx = list.findIndex((e) => e.parse_path === sp);
    if (idx < 0) return;
    tab.selection = new Set([sp]);
    tab.anchorIndex = idx;
    renderList();
    renderStatus();
    $("list-body").querySelector(`[data-path="${CSS.escape(sp)}"]`)?.scrollIntoView({ block: "nearest" });
  };

  if (cached) {
    // 命中缓存：立即渲染，后台继续拉最新数据
    pendingPath = null;
    tab.listing = cached;
    tab.selection.clear();
    tab.anchorIndex = -1;
    tab.filter = "";
    ($("search-input") as HTMLInputElement).value = "";
    if (opts.push) {
      tab.history.splice(tab.historyIndex + 1);
      tab.history.push(cached.parse_path);
      tab.historyIndex = tab.history.length - 1;
      pushed = true;
    }
    renderAll();
    applySelect();
    // 缓存已先行渲染，ShellBag 视图到达后若不同再局部重绘
    void viewPromise.then((v) => {
      if (token === navToken && v && tab.view !== v) {
        tab.view = v as ViewMode;
        renderHeader();
        renderList();
        renderViewButtons();
      }
    });
  } else {
    // 立即反馈：侧栏选中高亮切换 + 列表显示加载提示
    renderSidebar();
    $("list-body").innerHTML = '<div class="empty-hint">正在处理它...</div>';
  }
  try {
    const [listing, savedView] = await Promise.all([
      invoke<FolderListing>("list_folder", { path }),
      viewPromise,
    ]);
    // 期间又发起了新导航，丢弃旧结果
    if (token !== navToken) return false;
    if (savedView) tab.view = savedView as ViewMode;
    pendingPath = null;
    cacheListing(path, listing);
    tab.listing = listing;
    if (!cached) {
      tab.selection.clear();
      tab.anchorIndex = -1;
      tab.filter = "";
      ($("search-input") as HTMLInputElement).value = "";
    }
    if (opts.push && !pushed) {
      tab.history.splice(tab.historyIndex + 1);
      tab.history.push(listing.parse_path);
      tab.historyIndex = tab.history.length - 1;
    }
    renderAll();
    applySelect();
    void loadFolderIcon(listing.parse_path);
    void invoke("watch_folder", { path: listing.parse_path });
    return true;
  } catch (e) {
    console.error("navigate failed:", e);
    if (token === navToken && !cached) {
      // 失败：恢复原视图
      pendingPath = null;
      renderAll();
    }
    return false;
  }
}

// 加载当前文件夹自身图标（用于标签页）
async function loadFolderIcon(path: string) {
  if (iconCache.has(`32|${path}`)) { renderTabs(); return; }
  const icons = await invoke<(string | null)[]>("get_icons", {
    items: [{ path, is_folder: true, ext: "" }],
    size: 32,
  });
  if (icons[0]) iconCache.set(`32|${path}`, icons[0]);
  renderTabs();
}

async function refresh() {
  const tab = activeTab();
  const path = tab.listing?.parse_path ?? tab.history[tab.historyIndex];
  const keep = new Set(tab.selection);
  try {
    const listing = await invoke<FolderListing>("list_folder", { path });
    tab.listing = listing;
    cacheListing(path, listing);
    tab.selection = new Set(listing.entries.filter((e) => keep.has(e.parse_path)).map((e) => e.parse_path));
    renderAll();
  } catch { /* 文件夹可能已删除 */ }
}

// 若目标是当前目录的祖先，返回沿路径的直接子级（任何方式回到上级都选中来源文件夹）
function childOnPathTo(target: string): string | undefined {
  const bc = activeTab().listing?.breadcrumb ?? [];
  const idx = bc.findIndex((c) => c.parse_path === target);
  if (idx >= 0 && idx < bc.length - 1) return bc[idx + 1].parse_path;
  return undefined;
}

function goBack() {
  const tab = activeTab();
  if (tab.historyIndex > 0) {
    tab.historyIndex--;
    const target = tab.history[tab.historyIndex];
    void navigate(target, { push: false, selectPath: childOnPathTo(target) });
  }
}
function goForward() {
  const tab = activeTab();
  if (tab.historyIndex < tab.history.length - 1) {
    tab.historyIndex++;
    const target = tab.history[tab.historyIndex];
    void navigate(target, { push: false, selectPath: childOnPathTo(target) });
  }
}
function goUp() {
  const tab = activeTab();
  const bc = tab.listing?.breadcrumb ?? [];
  if (bc.length >= 2) {
    // 选中刚才所在的子文件夹
    const from = tab.listing!.parse_path;
    void navigate(bc[bc.length - 2].parse_path, { push: true, selectPath: from });
  }
}

/* ===================== 排序与过滤 ===================== */
const collator = new Intl.Collator("zh-CN", { numeric: true, sensitivity: "base" });

// 按"文件扩展名"开关计算显示名
function displayName(e: ShellEntry): string {
  const full = e.full_name || e.name;
  if (e.is_folder || settings.showExt) return full;
  const dot = full.lastIndexOf(".");
  return dot > 0 ? full.slice(0, dot) : full;
}

function sortedEntries(tab: Tab): ShellEntry[] {
  const l = tab.listing;
  if (!l) return [];
  let list = l.entries;
  if (!settings.showHidden) list = list.filter((e) => !e.is_hidden);
  if (tab.filter) {
    const f = tab.filter.toLowerCase();
    list = list.filter((e) => e.name.toLowerCase().includes(f));
  }
  const dir = tab.sortAsc ? 1 : -1;
  return [...list].sort((a, b) => {
    if (a.sort_as_folder !== b.sort_as_folder) return a.sort_as_folder ? -1 : 1;
    switch (tab.sortKey) {
      case "date": return (a.date_modified - b.date_modified) * dir || collator.compare(a.name, b.name);
      case "created": return (a.date_created - b.date_created) * dir || collator.compare(a.name, b.name);
      case "type": return collator.compare(a.type_text, b.type_text) * dir || collator.compare(a.name, b.name);
      case "size": return ((a.size ?? 0) - (b.size ?? 0)) * dir || collator.compare(a.name, b.name);
      default: return collator.compare(a.name, b.name) * dir;
    }
  });
}

/* ===================== 渲染 ===================== */
let rowIconEls = new Map<string, HTMLImageElement>();
const rowIconMap = () => rowIconEls;

// 项目元素选择器（空白判定/框选命中共用）
const ITEM_SELECTOR = ".row, .grid-item, .list-item, .tile-item, .content-row, .drive-card, .item-check";
// 框选拖拽结束后抑制紧随的空白 click 清选
let suppressBlankClick = false;

/* -------- 鼠标框选（rubber band，与资源管理器一致） -------- */
function setupMarquee() {
  const body = $("list-body");
  body.addEventListener("mousedown", (ev) => {
    if (ev.button !== 0) return;
    const t = ev.target as HTMLElement;
    if (t.closest(ITEM_SELECTOR) || t.closest(".pc-group-header")) return;
    const tab = activeTab();
    const bodyRect = () => body.getBoundingClientRect();
    const r0 = bodyRect();
    // 起点（内容坐标系，含滚动）
    const startX = ev.clientX - r0.left + body.scrollLeft;
    const startY = ev.clientY - r0.top + body.scrollTop;
    const base = new Set(tab.selection);
    const additive = ev.ctrlKey;
    let started = false;
    let box: HTMLElement | null = null;
    let lastKey = "";

    const applyRect = (e: MouseEvent) => {
      const r = bodyRect();
      // 边缘自动滚动
      if (e.clientY > r.bottom - 8) body.scrollTop += Math.min(24, e.clientY - r.bottom + 8);
      else if (e.clientY < r.top + 8) body.scrollTop -= Math.min(24, r.top + 8 - e.clientY);
      const curX = e.clientX - r.left + body.scrollLeft;
      const curY = e.clientY - r.top + body.scrollTop;
      const x1 = Math.min(startX, curX), x2 = Math.max(startX, curX);
      const y1 = Math.min(startY, curY), y2 = Math.max(startY, curY);
      if (!started) {
        if (Math.abs(curX - startX) < 4 && Math.abs(curY - startY) < 4) return;
        started = true;
        box = document.createElement("div");
        box.className = "marquee";
        body.append(box);
      }
      box!.style.left = `${x1}px`;
      box!.style.top = `${y1}px`;
      box!.style.width = `${x2 - x1}px`;
      box!.style.height = `${y2 - y1}px`;

      // 命中测试：项目矩形与框选矩形相交
      const hit = new Set<string>();
      body.querySelectorAll<HTMLElement>("[data-path]").forEach((el) => {
        const er = el.getBoundingClientRect();
        const ex1 = er.left - r.left + body.scrollLeft;
        const ey1 = er.top - r.top + body.scrollTop;
        const ex2 = ex1 + er.width, ey2 = ey1 + er.height;
        if (ex1 < x2 && ex2 > x1 && ey1 < y2 && ey2 > y1) hit.add(el.dataset.path!);
      });
      const sel = additive ? new Set([...base, ...hit]) : hit;
      const key = `${sel.size}:${[...sel].sort().join("|")}`;
      if (key === lastKey) return;
      lastKey = key;
      tab.selection = sel;
      body.querySelectorAll<HTMLElement>("[data-path]").forEach((el) => {
        el.classList.toggle("selected", sel.has(el.dataset.path!));
      });
      renderStatus();
    };

    const up = () => {
      document.removeEventListener("mousemove", applyRect);
      box?.remove();
      if (started) suppressBlankClick = true;
    };
    document.addEventListener("mousemove", applyRect);
    document.addEventListener("mouseup", up, { once: true });
  });
}

/* -------- 原生拖拽：drop 目标命中测试与高亮（与资源管理器一致） -------- */
function setupNativeDnD() {
  interface Target { kind: string; path: string; fs: string | null; name: string; el: HTMLElement | null }
  let cur: Target | null = null;
  let curKey = "";
  let springEl: HTMLElement | null = null;
  let springTimer = 0;

  const clearSpring = () => {
    clearTimeout(springTimer);
    springTimer = 0;
    springEl = null;
  };
  const clearHighlight = () => {
    cur?.el?.classList.remove("drop-target");
    cur = null;
    curKey = "";
    clearSpring();
  };
  const report = (t: Target | null) => {
    const key = t ? `${t.kind}|${t.path}` : "none";
    if (key === curKey) return;
    cur?.el?.classList.remove("drop-target");
    cur = t;
    curKey = key;
    // 拖拽源自身不高亮（后端也会判定为禁止）
    const isSelf = t?.kind === "item" && draggingPaths.includes(t.path);
    if (!isSelf) t?.el?.classList.add("drop-target");
    void invoke("update_drop_target", {
      kind: t?.kind ?? "none",
      parsePath: t?.path ?? "",
      fsPath: t?.fs ?? null,
      name: t?.name ?? "",
    });
    // spring-load：悬停带折叠箭头的侧栏项 ≥800ms 自动展开
    clearSpring();
    if (t?.el?.classList.contains("side-item")) {
      const exp = t.el.querySelector<HTMLElement>(".side-expander");
      if (exp && exp.innerHTML.length > 0 && exp.textContent !== "\uE70D") {
        springEl = t.el;
        springTimer = window.setTimeout(() => {
          if (springEl && curKey === key) exp.click();
        }, 800);
      }
    }
  };

  // 虚拟命名空间根（此电脑/网络/快速访问根等）不可作为放置目标
  const isVirtualPath = (p: string) => p === "" || p.startsWith("::") || p.startsWith("shell:");

  const hitTest = (px: number, py: number): Target | null => {
    const x = px / devicePixelRatio;
    const y = py / devicePixelRatio;
    const el = document.elementFromPoint(x, y) as HTMLElement | null;
    if (!el) return null;

    // 1. 列表中的文件夹项（含此电脑驱动器卡片）
    const item = el.closest<HTMLElement>(ITEM_SELECTOR);
    if (item?.dataset.path && item.dataset.folder) {
      return { kind: "item", path: item.dataset.path, fs: item.dataset.fs ?? null, name: item.dataset.dropName ?? "", el: item };
    }

    // 2. 侧栏项（快速访问/驱动器/树节点）
    const side = el.closest<HTMLElement>(".side-item");
    if (side?.dataset.dropPath) {
      // 快速访问区内：项的上下边缘 → 固定到快速访问（与资源管理器插入行为一致）
      if (side.closest("#qa-zone")) {
        const r = side.getBoundingClientRect();
        if (y < r.top + r.height * 0.25 || y > r.bottom - r.height * 0.25) {
          return { kind: "pin", path: "", fs: null, name: "快速访问", el: $("qa-zone") };
        }
      }
      const p = side.dataset.dropPath;
      if (!isVirtualPath(p) || side.dataset.dropFs) {
        return { kind: "item", path: p, fs: side.dataset.dropFs ?? null, name: side.dataset.dropName ?? "", el: side };
      }
      return null;
    }

    // 3. 快速访问区空白 → 固定到快速访问
    const qa = el.closest<HTMLElement>("#qa-zone");
    if (qa) return { kind: "pin", path: "", fs: null, name: "快速访问", el: qa };

    // 4. 面包屑段
    const crumb = el.closest<HTMLElement>(".crumb");
    if (crumb?.dataset.dropPath && !isVirtualPath(crumb.dataset.dropPath)) {
      const p = crumb.dataset.dropPath;
      const fs = /^[a-zA-Z]:[\\/]|^\\\\/.test(p) ? p : null;
      return { kind: "item", path: p, fs, name: crumb.dataset.dropName ?? "", el: crumb };
    }

    // 5. 列表空白 → 当前文件夹背景
    const body = el.closest<HTMLElement>("#list-body");
    if (body) {
      const listing = activeTab().listing;
      if (listing && !isVirtualPath(listing.parse_path)) {
        const fs = /^[a-zA-Z]:[\\/]|^\\\\/.test(listing.parse_path) ? listing.parse_path : null;
        return { kind: "background", path: listing.parse_path, fs, name: listing.folder_name, el: body };
      }
    }
    return null;
  };

  const autoScroll = (py: number) => {
    const body = $("list-body");
    const r = body.getBoundingClientRect();
    const y = py / devicePixelRatio;
    if (y > r.bottom - 24 && y < r.bottom + 4) body.scrollTop += Math.min(24, y - r.bottom + 24);
    else if (y < r.top + 24 && y > r.top - 4) body.scrollTop -= Math.min(24, r.top + 24 - y);
  };

  void listen<{ x: number; y: number }>("fs-drag-enter", ({ payload }) => {
    report(hitTest(payload.x, payload.y));
  });
  void listen<{ x: number; y: number }>("fs-drag-over", ({ payload }) => {
    autoScroll(payload.y);
    report(hitTest(payload.x, payload.y));
  });
  void listen("fs-drag-leave", () => clearHighlight());
  void listen("fs-drag-drop", () => {
    clearHighlight();
    dragSuppressClickUntil = performance.now() + 400;
  });
  void listen("fs-drag-finished", () => {
    clearHighlight();
    draggingPaths = [];
    dragSuppressClickUntil = performance.now() + 400;
  });
  // 拖拽固定到快速访问完成 → 刷新侧栏
  void listen("fs-quick-access-changed", () => void loadSidebar());
}

// 缩略图批量加载（中/大/超大图标视图与预览窗格）
async function loadThumbs(entries: ShellEntry[], targets: () => Map<string, HTMLImageElement>, size: number) {
  const missing = entries.filter((e) => !iconCache.has(`t${size}|${e.parse_path}`));
  const CHUNK = 24;
  for (let i = 0; i < missing.length; i += CHUNK) {
    const chunk = missing.slice(i, i + CHUNK);
    const thumbs = await invoke<(string | null)[]>("get_thumbnails", {
      paths: chunk.map((e) => e.parse_path),
      size,
    });
    chunk.forEach((e, j) => {
      if (thumbs[j]) iconCache.set(`t${size}|${e.parse_path}`, thumbs[j]!);
    });
    const map = targets();
    chunk.forEach((e) => {
      const img = map.get(e.parse_path);
      const t = iconCache.get(`t${size}|${e.parse_path}`);
      if (img && t) img.src = t;
    });
  }
}

function renderAll() {
  renderTabs();
  renderNav();
  renderBreadcrumb();
  renderSidebar();
  renderHeader();
  renderList();
  renderStatus();
  renderViewButtons();
  applyGlobalSettings();
  const name = activeTab().listing?.folder_name ?? "";
  ($("search-input") as HTMLInputElement).placeholder = name ? `在 ${name} 中搜索` : "搜索";
  void appWindow.setTitle(name || "FileSpace");
}

// 应用全局显示设置（导航窗格/紧凑视图/窗格）
function applyGlobalSettings() {
  const sb = document.querySelector<HTMLElement>(".sidebar");
  const rs = document.querySelector<HTMLElement>(".sidebar-resizer");
  if (sb) sb.style.display = settings.navPane ? "" : "none";
  if (rs) rs.style.display = settings.navPane ? "" : "none";
  document.body.classList.toggle("compact", settings.compact);
  renderSidePane();
}

function renderTabs() {
  const strip = $("tabstrip");
  strip.innerHTML = "";
  tabs.forEach((tab, i) => {
    const el = document.createElement("div");
    el.className = "tab" + (i === activeTabIdx ? " active" : "");
    const icon = document.createElement("img");
    icon.className = "tab-icon";
    const p = tab.listing?.parse_path;
    icon.src = (p && getIcon(p, 32)) || folderSvg();
    const title = document.createElement("span");
    title.className = "tab-title";
    title.textContent = tab.listing?.folder_name || "新标签页";
    el.append(icon, title);
    if (tabs.length > 1 || true) {
      const close = document.createElement("button");
      close.className = "tab-close";
      close.innerHTML = '<span class="fluent">&#xE8BB;</span>';
      close.onclick = (ev) => { ev.stopPropagation(); closeTab(i); };
      el.append(close);
    }
    el.onclick = () => { activeTabIdx = i; renderAll(); };
    el.onauxclick = (ev) => { if (ev.button === 1) closeTab(i); };
    strip.append(el);
  });
}

function folderSvg(): string {
  return "data:image/svg+xml," + encodeURIComponent(
    '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><path fill="#f8d775" d="M1 3.5C1 2.7 1.7 2 2.5 2h3.6l1.6 1.5h5.8c.8 0 1.5.7 1.5 1.5v7.5c0 .8-.7 1.5-1.5 1.5h-11c-.8 0-1.5-.7-1.5-1.5z"/></svg>');
}

function renderNav() {
  const tab = activeTab();
  ($("nav-back") as HTMLButtonElement).disabled = tab.historyIndex <= 0;
  ($("nav-fwd") as HTMLButtonElement).disabled = tab.historyIndex >= tab.history.length - 1;
  ($("nav-up") as HTMLButtonElement).disabled = (tab.listing?.breadcrumb.length ?? 0) < 2;
}

// 桌面根子项缓存（地址栏最前的根下拉）
let desktopItems: ShellEntry[] | null = null;

function renderBreadcrumb() {
  const bc = $("breadcrumb");
  bc.innerHTML = "";
  const listing = activeTab().listing;
  if (!listing) return;

  // 点击分隔箭头：下拉列出该段的子文件夹供快速切换（与资源管理器一致）
  const chevDropdown = async (chev: HTMLElement, parsePath: string) => {
    if (suppressAnchor === chev) { suppressAnchor = null; return; }
    try {
      const l = await invoke<FolderListing>("list_folder", { path: parsePath });
      const folders = l.entries.filter((e) => e.is_folder);
      if (folders.length === 0) {
        showDropdown(chev, [{ label: "（无子文件夹）", disabled: true }]);
        return;
      }
      showDropdown(chev, folders.map((f) => ({
        label: stripNetworkSuffix(f.name),
        onClick: () => void navigate(f.parse_path),
      })));
    } catch { /* 该段不可枚举 */ }
  };

  // 最前：路径根节点图标（磁盘路径下即"此电脑"显示器图标，与资源管理器一致）+ 桌面根下拉
  const rootCrumb = listing.breadcrumb[0];
  const locIcon = document.createElement("img");
  locIcon.className = "crumb-loc-icon";
  locIcon.src = (rootCrumb && getIcon(rootCrumb.parse_path, 32)) || getIcon(listing.parse_path, 32) || folderSvg();
  if (rootCrumb && !getIcon(rootCrumb.parse_path, 32)) {
    void loadIcons(
      [{ parse_path: rootCrumb.parse_path, sort_as_folder: true, ext: "" } as ShellEntry],
      () => new Map([[rootCrumb.parse_path, locIcon]]),
    );
  }
  bc.append(locIcon);
  const rootChev = document.createElement("span");
  rootChev.className = "fluent crumb-chev";
  rootChev.innerHTML = "&#xE76C;";
  rootChev.onclick = async (ev) => {
    ev.stopPropagation();
    if (suppressAnchor === rootChev) { suppressAnchor = null; return; }
    if (!desktopItems) {
      try {
        desktopItems = await invoke<ShellEntry[]>("get_desktop_items");
      } catch { desktopItems = []; }
    }
    const folders = desktopItems.filter((e) => e.is_folder);
    if (folders.length === 0) return;
    showDropdown(rootChev, folders.map((f) => ({
      label: stripNetworkSuffix(f.name),
      onClick: () => void navigate(f.parse_path),
    })));
  };
  bc.append(rootChev);

  listing.breadcrumb.forEach((c, i) => {
    const el = document.createElement("div");
    el.className = "crumb";
    el.textContent = c.name;
    el.dataset.dropPath = c.parse_path;
    el.dataset.dropName = c.name;
    el.onclick = (ev) => {
      ev.stopPropagation();
      // 点面包屑回到祖先目录也选中沿路径的子级
      void navigate(c.parse_path, { push: true, selectPath: childOnPathTo(c.parse_path) });
    };
    bc.append(el);
    const isLast = i === listing.breadcrumb.length - 1;
    const chev = document.createElement("span");
    chev.className = "fluent crumb-chev";
    // 末段用下拉箭头，中间段用 > 分隔符
    chev.innerHTML = isLast ? "&#xE70D;" : "&#xE76C;";
    chev.onclick = (ev) => {
      ev.stopPropagation();
      void chevDropdown(chev, c.parse_path);
    };
    bc.append(chev);
  });
}

function sideItem(entry: ShellEntry, opts: { pin?: boolean; indent?: number; expander?: "open" | "closed" | "none" }): HTMLElement {
  const el = document.createElement("div");
  el.className = "side-item";
  // 乐观高亮：导航进行中时以目标路径为准
  const current = pendingPath ?? activeTab().listing?.parse_path;
  if (current === entry.parse_path) el.classList.add("selected");
  el.style.paddingLeft = `${8 + (opts.indent ?? 0) * 16}px`;
  const exp = document.createElement("span");
  exp.className = "fluent side-expander";
  exp.innerHTML = opts.expander === "open" ? "&#xE70D;" : opts.expander === "closed" ? "&#xE76C;" : "";
  const icon = document.createElement("img");
  icon.className = "side-icon";
  icon.src = getIcon(entry.parse_path, 32) || folderSvg();
  const label = document.createElement("span");
  label.className = "side-label";
  label.textContent = entry.name;
  el.append(exp, icon, label);
  if (opts.pin) {
    const pin = document.createElement("span");
    pin.className = "fluent side-pin";
    pin.innerHTML = "&#xE718;";
    el.append(pin);
  }
  el.dataset.dropPath = entry.parse_path;
  if (entry.fs_path) el.dataset.dropFs = entry.fs_path;
  el.dataset.dropName = entry.name;
  el.onmousedown = (ev) => {
    if (ev.button !== 0 || (ev.target as HTMLElement).closest(".side-expander, .rename-input")) return;
    beginDragWatch(ev, () => [entry.parse_path]);
  };
  el.onclick = () => {
    if (consumeDragClickSuppress()) return;
    void navigate(entry.parse_path);
  };
  el.oncontextmenu = (ev) => {
    ev.preventDefault();
    ev.stopPropagation();
    void showSideItemMenu(ev.clientX, ev.clientY, entry, el);
  };
  return el;
}

let thisPcExpanded = true;
// 侧栏递归树状态：展开集合 / 子节点缓存 / 已知无子文件夹 / 正在加载
const sideExpanded = new Set<string>();
const sideChildren = new Map<string, ShellEntry[]>();
const sideHasKids = new Map<string, boolean>();
const sideFetching = new Set<string>();

// 通用递归树节点：磁盘/文件夹/网络/Linux 均可逐级展开（与资源管理器导航窗格一致）
function renderTreeNode(sb: HTMLElement, entry: ShellEntry, indent: number) {
  const key = entry.parse_path;
  const expanded = sideExpanded.has(key);
  const noKids = sideHasKids.get(key) === false;
  const el = sideItem(entry, { indent, expander: noKids ? "none" : expanded ? "open" : "closed" });
  const lbl = el.querySelector(".side-label");
  if (lbl) lbl.textContent = stripNetworkSuffix(entry.name);
  el.querySelector(".side-expander")!.addEventListener("click", async (ev) => {
    ev.stopPropagation();
    if (sideHasKids.get(key) === false) return;
    if (sideExpanded.has(key)) {
      sideExpanded.delete(key);
      renderSidebar();
      return;
    }
    sideExpanded.add(key);
    renderSidebar();
    if (!sideChildren.has(key) && !sideFetching.has(key)) {
      sideFetching.add(key);
      try {
        const l = await invoke<FolderListing>("list_folder", { path: key });
        const kids = l.entries.filter((e) => e.is_folder);
        sideChildren.set(key, kids);
        if (kids.length === 0) {
          sideHasKids.set(key, false);
          sideExpanded.delete(key);
        }
        renderSidebar();
        void loadIcons(kids, () => new Map()).then(renderSidebar);
      } catch {
        sideChildren.set(key, []);
        sideHasKids.set(key, false);
        sideExpanded.delete(key);
        renderSidebar();
      } finally {
        sideFetching.delete(key);
      }
    }
  });
  sb.append(el);
  if (expanded) {
    const kids = (sideChildren.get(key) ?? []).filter((e) => settings.showHidden || !e.is_hidden);
    for (const kid of kids) renderTreeNode(sb, kid, indent + 1);
  }
}

function renderSidebar() {
  const sb = $("sidebar");
  sb.innerHTML = "";
  if (!sidebar) return;

  const qaZone = document.createElement("div");
  qaZone.id = "qa-zone";
  for (const qa of sidebar.quick_access) {
    qaZone.append(sideItem(qa, { pin: qa.pinned, expander: "none" }));
  }
  sb.append(qaZone);
  const div = document.createElement("div");
  div.className = "side-divider";
  sb.append(div);

  // 此电脑
  const pcEntry: ShellEntry = {
    name: "此电脑", parse_path: THIS_PC, fs_path: null, is_folder: true, sort_as_folder: true,
    is_hidden: false, size: null, size_text: "", date_modified: 0, date_text: "", type_text: "", ext: "",
  } as ShellEntry;
  const pcEl = sideItem(pcEntry, { expander: thisPcExpanded ? "open" : "closed" });
  pcEl.querySelector(".side-expander")!.addEventListener("click", (ev) => {
    ev.stopPropagation();
    thisPcExpanded = !thisPcExpanded;
    renderSidebar();
  });
  sb.append(pcEl);
  if (thisPcExpanded) {
    for (const d of sidebar.drives) {
      renderTreeNode(sb, d, 1);
    }
  }

  // 网络 / Linux(WSL)（与资源管理器导航窗格一致）
  if (sidebar.network) renderTreeNode(sb, sidebar.network, 0);
  if (sidebar.linux) renderTreeNode(sb, sidebar.linux, 0);
}

const COLS: { key: Tab["sortKey"]; label: string }[] = [
  { key: "name", label: "名称" },
  { key: "date", label: "修改日期" },
  { key: "created", label: "创建日期" },
  { key: "type", label: "类型" },
  { key: "size", label: "大小" },
];

// 列可见性（名称列固定显示），表头右键菜单切换，持久化
const colVisible: Record<SortKey, boolean> = (() => {
  const def = { name: true, date: true, created: false, type: true, size: true };
  try {
    const saved = JSON.parse(localStorage.getItem("colVisible") ?? "");
    if (saved && typeof saved === "object") return { ...def, ...saved, name: true };
  } catch { /* 首次运行 */ }
  return def;
})();

const visibleCols = () => COLS.filter((c) => colVisible[c.key]);

function colValue(e: ShellEntry, key: SortKey): string {
  switch (key) {
    case "date": return e.date_text;
    case "created": return e.date_created_text;
    case "type": return e.type_text;
    case "size": return e.size_text;
    default: return "";
  }
}

const COL_DEFAULT_W: Record<SortKey, number> = { name: 0, date: 150, created: 150, type: 130, size: 100 };

// 列宽（可拖拽调整，持久化）；name 为 0 表示自适应弹性列
const colWidths: Record<Tab["sortKey"], number> = (() => {
  try {
    const saved = JSON.parse(localStorage.getItem("colWidths") ?? "");
    if (saved && typeof saved === "object") return { ...COL_DEFAULT_W, ...saved };
  } catch { /* 首次运行 */ }
  return { ...COL_DEFAULT_W };
})();

function colStyle(key: Tab["sortKey"]): string {
  const w = colWidths[key];
  if (key === "name" && w === 0) return "flex:1 1 40%;min-width:200px";
  return `width:${w}px;flex-shrink:0`;
}

const COL_CLASS: Record<Tab["sortKey"], string> = {
  name: "cell-name", date: "cell-date", created: "cell-created", type: "cell-type", size: "cell-size",
};

// 拖拽中实时同步表头与所有行的列宽（避免整表重渲染）
function applyColWidth(key: Tab["sortKey"]) {
  const style = colStyle(key);
  document.querySelectorAll<HTMLElement>(`.col-${key}, .${COL_CLASS[key]}`).forEach((el) => {
    el.setAttribute("style", el.classList.contains("col-header") ? `${style};position:relative` : style);
  });
  // 名称列在弹性/固定间切换时，行宽模式需要重算
  if (key === "name") {
    const fit = colWidths.name !== 0;
    document.querySelectorAll<HTMLElement>(".row").forEach((r) => {
      r.style.width = fit ? "max-content" : "";
    });
  }
}

function renderHeader() {
  const tab = activeTab();
  const h = $("list-header");
  const show = tab.view === "details" && tab.listing?.parse_path !== THIS_PC && tab.listing?.parse_path !== LINUX_WSL;
  h.style.display = show ? "" : "none";
  h.innerHTML = "";
  if (!show) return;
  // 表头右键：列可见性菜单（与资源管理器一致）
  h.oncontextmenu = (ev) => {
    ev.preventDefault();
    ev.stopPropagation();
    showHeaderMenu(ev.clientX, ev.clientY);
  };
  for (const col of visibleCols()) {
    const el = document.createElement("div");
    el.className = `col-header col-${col.key}`;
    el.setAttribute("style", `${colStyle(col.key)};position:relative`);
    const label = document.createElement("span");
    label.className = "col-label";
    label.textContent = col.label;
    el.append(label);
    // 排序指示符：居中置顶（与资源管理器一致）
    if (tab.sortKey === col.key) {
      const g = document.createElement("span");
      g.className = "fluent sort-glyph";
      g.innerHTML = tab.sortAsc ? "&#xE70E;" : "&#xE70D;";
      el.append(g);
    }
    el.onclick = () => {
      if (tab.sortKey === col.key) tab.sortAsc = !tab.sortAsc;
      else { tab.sortKey = col.key; tab.sortAsc = true; }
      renderHeader();
      renderList();
    };
    // 列分割线拖拽调宽
    const handle = document.createElement("span");
    handle.className = "col-resize-handle";
    handle.onmousedown = (ev) => {
      ev.preventDefault();
      ev.stopPropagation();
      const startX = ev.clientX;
      const startW = el.getBoundingClientRect().width;
      document.body.style.cursor = "col-resize";
      let moved = false;
      const move = (e: MouseEvent) => {
        moved = true;
        colWidths[col.key] = Math.min(800, Math.max(60, Math.round(startW + e.clientX - startX)));
        applyColWidth(col.key);
      };
      const up = () => {
        document.removeEventListener("mousemove", move);
        document.body.style.cursor = "";
        if (moved) localStorage.setItem("colWidths", JSON.stringify(colWidths));
      };
      document.addEventListener("mousemove", move);
      document.addEventListener("mouseup", up, { once: true });
    };
    handle.onclick = (ev) => ev.stopPropagation();
    el.append(handle);
    h.append(el);
  }
}

function renderList() {
  const tab = activeTab();
  const body = $("list-body");
  body.innerHTML = "";
  rowIconEls = new Map();
  const entries = sortedEntries(tab);

  const isThisPc = tab.listing?.parse_path === THIS_PC;
  const isLinux = tab.listing?.parse_path === LINUX_WSL;
  if (isThisPc) {
    renderThisPc(body, tab, entries);
  } else if (isLinux) {
    renderCardGrid(body, tab, entries);
  } else {
    switch (tab.view) {
      case "details": renderDetailRows(body, tab, entries); break;
      case "tiles": renderTiles(body, tab, entries); break;
      case "content": renderContent(body, tab, entries); break;
      case "list": renderListView(body, tab, entries); break;
      default: renderIconGrid(body, tab, entries); break;
    }
  }

  if (entries.length === 0) {
    const hint = document.createElement("div");
    hint.className = "empty-hint";
    hint.textContent = tab.filter ? "没有与搜索条件匹配的项目。" : "此文件夹为空。";
    body.append(hint);
  }

  // 空白处：目标不在任何项目元素内即视为空白（覆盖所有视图/磁盘卡片/Linux 卡片）
  body.oncontextmenu = (ev) => {
    const t = ev.target as HTMLElement;
    if (t.closest(ITEM_SELECTOR)) return; // 项目自身的右键由各自 handler 处理
    ev.preventDefault();
    tab.selection.clear();
    renderList();
    renderStatus();
    void showBackgroundMenu(ev.clientX, ev.clientY);
  };
  body.onclick = (ev) => {
    // 框选拖拽结束后的 click 不清选中
    if (suppressBlankClick) { suppressBlankClick = false; return; }
    const t = ev.target as HTMLElement;
    if (t.closest(ITEM_SELECTOR)) return;
    if (t.closest(".pc-group-header")) return; // 折叠分组不清选中
    if (tab.selection.size === 0) return;
    tab.selection.clear();
    tab.anchorIndex = -1;
    renderList();
    renderStatus();
  };
}

// 公共：选中/双击/右键/剪切态 绑定
function bindItemEvents(el: HTMLElement, e: ShellEntry, idx: number, tab: Tab) {
  el.dataset.path = e.parse_path;
  if (e.is_folder) el.dataset.folder = "1";
  if (e.fs_path) el.dataset.fs = e.fs_path;
  el.dataset.dropName = e.name;
  if (tab.selection.has(e.parse_path)) el.classList.add("selected");
  if (cutPaths.has(e.parse_path)) el.classList.add("cut");
  // 按下前是否已是唯一选中项（"再次单击名称进入重命名"判定，与资源管理器一致）
  let preSelected = false;
  el.onmousedown = (ev) => {
    if (ev.button !== 0) return;
    // 重命名输入框内：不拢选不拖拽，交给输入框自行处理文字选择
    if ((ev.target as HTMLElement).closest(".rename-input")) return;
    preSelected = tab.selection.size === 1 && tab.selection.has(e.parse_path);
    // Explorer 语义：mousedown 即选中未选中项；已选中项保持多选以便整体拖拽
    if (!tab.selection.has(e.parse_path) && !ev.ctrlKey && !ev.shiftKey) {
      selectRow(idx, e, ev);
    }
    beginDragWatch(ev, () => {
      const t = activeTab();
      return t.selection.has(e.parse_path) ? [...t.selection] : [e.parse_path];
    });
  };
  el.onclick = (ev) => {
    ev.stopPropagation();
    if ((ev.target as HTMLElement).closest(".rename-input")) return;
    if (consumeDragClickSuppress()) return;
    window.clearTimeout(renameTimer);
    // 已选中项上再次单击名称（非双击/无修饰键）：延时进入重命名，与资源管理器一致
    const armRename = preSelected && ev.detail === 1 && !ev.ctrlKey && !ev.shiftKey
      && isNameTarget(ev.target as HTMLElement);
    selectRow(idx, e, ev);
    if (armRename) renameTimer = window.setTimeout(() => beginItemRename(e), 500);
  };
  el.ondblclick = () => {
    // 双击：取消待触发的单击重命名，执行打开
    window.clearTimeout(renameTimer);
    void openEntry(e);
  };
  el.oncontextmenu = (ev) => {
    ev.preventDefault();
    ev.stopPropagation();
    window.clearTimeout(renameTimer);
    if (!tab.selection.has(e.parse_path)) {
      tab.selection = new Set([e.parse_path]);
      tab.anchorIndex = idx;
      renderList();
      renderStatus();
    }
    void showItemMenu(ev.clientX, ev.clientY);
  };
}

// "选中后再次单击重命名"延时器（500ms，与系统双击间隔一致）
let renameTimer: number | undefined;

// 单击目标是否为名称文字（各视图的名称元素；资源管理器仅点名称才触发重命名）
const isNameTarget = (t: HTMLElement) =>
  t.matches(".grid-name, .list-name, .tile-name, .content-name, .drive-name") ||
  (t.tagName === "SPAN" && !!t.closest(".cell-name"));

// 触发单击重命名：磁盘卡片改卷标，文件系统项通用重命名；触发前重验选中状态
function beginItemRename(e: ShellEntry) {
  const t = activeTab();
  if (!(t.selection.size === 1 && t.selection.has(e.parse_path))) return;
  if (document.querySelector(".rename-input")) return; // 已在重命名中
  if (/^[A-Za-z]:\\$/.test(e.parse_path)) { startDriveRename(e); return; }
  if (!e.fs_path) return; // 虚拟项不支持重命名
  startRename();
}

/* -------- 原生拖拽：源启动（4px 阈值，与资源管理器一致） -------- */
let dragSuppressClickUntil = 0;
let draggingPaths: string[] = [];
function consumeDragClickSuppress(): boolean {
  return performance.now() < dragSuppressClickUntil;
}

function beginDragWatch(ev: MouseEvent, getPaths: () => string[]) {
  const sx = ev.screenX, sy = ev.screenY;
  const move = (mv: MouseEvent) => {
    if (Math.abs(mv.screenX - sx) < 4 && Math.abs(mv.screenY - sy) < 4) return;
    cleanup();
    const paths = getPaths().filter(Boolean);
    if (paths.length === 0) return;
    dragSuppressClickUntil = performance.now() + 600;
    draggingPaths = paths;
    void invoke("start_drag", { paths });
  };
  const cleanup = () => {
    document.removeEventListener("mousemove", move);
    document.removeEventListener("mouseup", cleanup);
  };
  document.addEventListener("mousemove", move);
  document.addEventListener("mouseup", cleanup);
}

// 公共：项目复选框
function makeCheckbox(e: ShellEntry, tab: Tab): HTMLInputElement {
  const cb = document.createElement("input");
  cb.type = "checkbox";
  cb.className = "item-check";
  cb.checked = tab.selection.has(e.parse_path);
  cb.onclick = (ev) => {
    ev.stopPropagation();
    if (cb.checked) tab.selection.add(e.parse_path);
    else tab.selection.delete(e.parse_path);
    renderList();
    renderStatus();
  };
  return cb;
}

function makeItemIcon(e: ShellEntry, size: number, thumb: boolean): HTMLImageElement {
  const img = document.createElement("img");
  const cached = thumb ? iconCache.get(`t${size}|${e.parse_path}`) : getIcon(e.parse_path, size);
  img.src = cached || "data:image/gif;base64,R0lGODlhAQABAAAAACwAAAAAAQABAAA=";
  if (e.is_hidden) img.style.opacity = "0.55";
  rowIconEls.set(e.parse_path, img);
  return img;
}

function requestIcons(tab: Tab, entries: ShellEntry[]) {
  const cfg = VIEW_CFG[tab.view];
  if (tab.listing?.parse_path === THIS_PC) {
    void loadIcons(entries, rowIconMap, 48);
  } else if (cfg.thumb) {
    void loadThumbs(entries, rowIconMap, cfg.icon);
  } else {
    void loadIcons(entries, rowIconMap, cfg.icon);
  }
}

/* -------- 详细信息 -------- */
function renderDetailRows(body: HTMLElement, tab: Tab, entries: ShellEntry[]) {
  // 名称列为固定宽时，行宽收缩为各列总宽（不延伸到屏幕最右，与表头一致）
  const fitWidth = colWidths.name !== 0;
  entries.forEach((e, idx) => {
    const row = document.createElement("div");
    row.className = "row";
    if (fitWidth) row.style.width = "max-content";

    const nameCell = document.createElement("div");
    nameCell.className = "cell cell-name";
    nameCell.setAttribute("style", colStyle("name"));
    if (settings.checkboxes) nameCell.append(makeCheckbox(e, tab));
    const img = makeItemIcon(e, 32, false);
    const nm = document.createElement("span");
    nm.textContent = displayName(e);
    nm.style.overflow = "hidden";
    nm.style.textOverflow = "ellipsis";
    nameCell.append(img, nm);
    row.append(nameCell);

    // 其余列按可见性动态生成
    for (const col of visibleCols()) {
      if (col.key === "name") continue;
      const cell = document.createElement("div");
      cell.className = `cell ${COL_CLASS[col.key]}`;
      cell.setAttribute("style", colStyle(col.key));
      cell.textContent = colValue(e, col.key);
      row.append(cell);
    }

    bindItemEvents(row, e, idx, tab);
    body.append(row);
  });
  requestIcons(tab, entries);
}

/* -------- 图标网格（小/中/大/超大） -------- */
function renderIconGrid(body: HTMLElement, tab: Tab, entries: ShellEntry[]) {
  const cfg = VIEW_CFG[tab.view];
  const grid = document.createElement("div");
  grid.className = `icon-grid ig-${tab.view}`;
  entries.forEach((e, idx) => {
    const cell = document.createElement("div");
    cell.className = "grid-item";
    if (settings.checkboxes) cell.append(makeCheckbox(e, tab));
    const img = makeItemIcon(e, cfg.icon, cfg.thumb);
    const nm = document.createElement("span");
    nm.className = "grid-name";
    nm.textContent = displayName(e);
    nm.title = e.name;
    cell.append(img, nm);
    bindItemEvents(cell, e, idx, tab);
    grid.append(cell);
  });
  body.append(grid);
  requestIcons(tab, entries);
}

/* -------- 列表（多列流式） -------- */
function renderListView(body: HTMLElement, tab: Tab, entries: ShellEntry[]) {
  const cols = document.createElement("div");
  cols.className = "list-columns";
  entries.forEach((e, idx) => {
    const item = document.createElement("div");
    item.className = "list-item";
    if (settings.checkboxes) item.append(makeCheckbox(e, tab));
    const img = makeItemIcon(e, 16, false);
    const nm = document.createElement("span");
    nm.className = "list-name";
    nm.textContent = displayName(e);
    nm.title = e.name;
    item.append(img, nm);
    bindItemEvents(item, e, idx, tab);
    cols.append(item);
  });
  body.append(cols);
  requestIcons(tab, entries);
}

/* -------- 平铺 -------- */
function renderTiles(body: HTMLElement, tab: Tab, entries: ShellEntry[]) {
  const grid = document.createElement("div");
  grid.className = "tile-grid";
  entries.forEach((e, idx) => {
    const cell = document.createElement("div");
    cell.className = "tile-item";
    if (settings.checkboxes) cell.append(makeCheckbox(e, tab));
    const img = makeItemIcon(e, 48, false);
    const meta = document.createElement("div");
    meta.className = "tile-meta";
    const nm = document.createElement("div");
    nm.className = "tile-name";
    nm.textContent = displayName(e);
    nm.title = e.name;
    const l2 = document.createElement("div");
    l2.className = "tile-sub";
    l2.textContent = e.type_text;
    const l3 = document.createElement("div");
    l3.className = "tile-sub";
    l3.textContent = e.size_text;
    meta.append(nm, l2);
    if (e.size_text) meta.append(l3);
    cell.append(img, meta);
    bindItemEvents(cell, e, idx, tab);
    grid.append(cell);
  });
  body.append(grid);
  requestIcons(tab, entries);
}

/* -------- 内容 -------- */
function renderContent(body: HTMLElement, tab: Tab, entries: ShellEntry[]) {
  entries.forEach((e, idx) => {
    const row = document.createElement("div");
    row.className = "content-row";
    if (settings.checkboxes) row.append(makeCheckbox(e, tab));
    const img = makeItemIcon(e, 32, false);
    const mid = document.createElement("div");
    mid.className = "content-mid";
    const nm = document.createElement("div");
    nm.className = "content-name";
    nm.textContent = displayName(e);
    const sub = document.createElement("div");
    sub.className = "content-sub";
    sub.textContent = [e.type_text, e.size_text].filter(Boolean).join(" · ");
    mid.append(nm, sub);
    const date = document.createElement("div");
    date.className = "content-date";
    date.textContent = e.date_text ? `修改日期: ${e.date_text}` : "";
    row.append(img, mid, date);
    bindItemEvents(row, e, idx, tab);
    body.append(row);
  });
  requestIcons(tab, entries);
}

/* -------- Linux(WSL)：与此电脑一致的卡片布局（发行版卡片，无容量条） -------- */
// 去掉网络共享显示名的地址后缀，如 "Ubuntu (\\\\wsl.localhost)" -> "Ubuntu"
function stripNetworkSuffix(name: string): string {
  return name.replace(/\s*\(\\\\[^)]*\)\s*$/, "");
}

function renderCardGrid(body: HTMLElement, tab: Tab, entries: ShellEntry[]) {
  const grid = document.createElement("div");
  grid.className = "pc-group";
  entries.forEach((e, idx) => {
    const card = document.createElement("div");
    card.className = "drive-card";
    const img = makeItemIcon(e, 48, false);
    img.className = "drive-icon";
    const meta = document.createElement("div");
    meta.className = "drive-meta";
    const nm = document.createElement("div");
    nm.className = "drive-name";
    nm.textContent = stripNetworkSuffix(e.name);
    meta.append(nm);
    card.append(img, meta);
    bindItemEvents(card, e, idx, tab);
    grid.append(card);
  });
  body.append(grid);
  void loadIcons(entries, rowIconMap, 48);
}

/* -------- 此电脑：设备和驱动器 / 网络位置（带容量条） -------- */
// 网络位置默认折叠（与资源管理器一致）
const pcGroupCollapsed: Record<string, boolean> = { "网络位置": true };

function renderThisPc(body: HTMLElement, tab: Tab, entries: ShellEntry[]) {
  const isDrive = (e: ShellEntry) => {
    const p = e.parse_path;
    return p.length === 3 && p[1] === ":" && p.endsWith("\\");
  };
  const drives = entries.filter(isDrive);
  // 固定按盘符顺序 C、D、E…（与资源管理器一致）
  drives.sort((a, b) => a.parse_path.localeCompare(b.parse_path));
  const local = drives.filter((e) => !e.type_text.includes("网络"));
  const network = drives.filter((e) => e.type_text.includes("网络"));

  const makeGroup = (label: string, items: ShellEntry[]) => {
    if (items.length === 0) return;
    const header = document.createElement("div");
    header.className = "pc-group-header";
    const chev = document.createElement("span");
    chev.className = "fluent pc-chev";
    chev.innerHTML = pcGroupCollapsed[label] ? "&#xE76C;" : "&#xE70D;";
    const lbl = document.createElement("span");
    lbl.textContent = label;
    header.append(chev, lbl);
    header.onclick = () => {
      pcGroupCollapsed[label] = !pcGroupCollapsed[label];
      renderList();
    };
    body.append(header);
    if (pcGroupCollapsed[label]) return;

    const grid = document.createElement("div");
    grid.className = "pc-group pc-drives";
    items.forEach((e) => {
      const idx = sortedEntries(tab).indexOf(e);
      const card = document.createElement("div");
      card.className = "drive-card pc-tile";
      const img = makeItemIcon(e, 48, false);
      img.className = "drive-icon";
      const meta = document.createElement("div");
      meta.className = "drive-meta";
      const nm = document.createElement("div");
      nm.className = "drive-name";
      nm.textContent = e.name;
      meta.append(nm);
      if (e.drive_total && e.drive_free != null) {
        const used = e.drive_total - e.drive_free;
        const pct = Math.min(100, Math.max(0, (used / e.drive_total) * 100));
        const bar = document.createElement("div");
        bar.className = "drive-bar";
        const fill = document.createElement("div");
        fill.className = "drive-bar-fill" + (e.drive_free / e.drive_total < 0.1 ? " low" : "");
        fill.style.width = `${pct}%`;
        bar.append(fill);
        const txt = document.createElement("div");
        txt.className = "drive-text";
        txt.textContent = e.drive_text;
        meta.append(bar, txt);
      } else {
        const txt = document.createElement("div");
        txt.className = "drive-text";
        txt.textContent = e.type_text;
        meta.append(txt);
      }
      card.append(img, meta);
      bindItemEvents(card, e, idx, tab);
      // 锚点（焦点）项显示浅灰焦点框，与资源管理器一致
      if (idx === tab.anchorIndex && tab.selection.has(e.parse_path)) card.classList.add("focus");
      grid.append(card);
    });
    body.append(grid);
  };

  makeGroup("设备和驱动器", local);
  makeGroup("网络位置", network);
  void loadIcons(drives, rowIconMap, 48);
}

function selectRow(idx: number, e: ShellEntry, ev: MouseEvent) {
  const tab = activeTab();
  const entries = sortedEntries(tab);
  if (ev.shiftKey && tab.anchorIndex >= 0) {
    const [a, b] = [Math.min(tab.anchorIndex, idx), Math.max(tab.anchorIndex, idx)];
    tab.selection = new Set(entries.slice(a, b + 1).map((x) => x.parse_path));
  } else if (ev.ctrlKey) {
    if (tab.selection.has(e.parse_path)) tab.selection.delete(e.parse_path);
    else tab.selection.add(e.parse_path);
    tab.anchorIndex = idx;
  } else {
    tab.selection = new Set([e.parse_path]);
    tab.anchorIndex = idx;
  }
  renderList();
  renderStatus();
}

function renderStatus() {
  const tab = activeTab();
  const total = sortedEntries(tab).length;
  $("status-count").textContent = `${total} 个项目`;
  const sel = tab.selection.size;
  $("status-sel-wrap").style.display = sel > 0 ? "" : "none";
  $("status-sel").textContent = sel > 0 ? `选中 ${sel} 个项目` : "";
  updateCommandStates();
  renderSidePane();
}

// 虚拟位置（此电脑/网络/Linux/快速访问等 Shell 命名空间根）禁用无效按钮，与资源管理器一致
function isVirtualLocation(): boolean {
  const p = activeTab().listing?.parse_path ?? "";
  return p === "" || p.startsWith("::") || p.startsWith("shell:");
}

function updateCommandStates() {
  const virt = isVirtualLocation();
  const sel = activeTab().selection.size;
  const set = (id: string, disabled: boolean) => {
    ($(id) as HTMLButtonElement).disabled = disabled;
  };
  set("cmd-new", virt);
  set("cmd-paste", virt);
  set("cmd-cut", virt || sel === 0);
  set("cmd-copy", virt || sel === 0);
  set("cmd-delete", virt || sel === 0);
  set("cmd-share", virt || sel === 0);
  set("cmd-rename", virt || sel !== 1);
}

/* ===================== 详细信息/预览窗格 ===================== */
let sidePaneToken = 0;
const IMAGE_EXTS = new Set(["jpg", "jpeg", "png", "gif", "bmp", "webp", "tif", "tiff", "ico", "heic", "avif", "jxr"]);

// 资源管理器风格字节数（3 位有效数字，如 "77.8 KB"）
function formatBytes(n: number): string {
  if (n < 1024) return `${n} 字节`;
  const units = ["KB", "MB", "GB", "TB"];
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
  const s = v >= 100 ? v.toFixed(0) : v >= 10 ? v.toFixed(1) : v.toFixed(2);
  return `${s} ${units[i]}`;
}

function parentDir(p: string | null): string {
  if (!p) return "";
  const sep = p.lastIndexOf("\\");
  if (sep <= 0) return "";
  const dir = p.slice(0, sep);
  return dir.length === 2 && dir[1] === ":" ? dir + "\\" : dir;
}

function paneRow(k: string, v: string): HTMLElement {
  const row = document.createElement("div");
  row.className = "pane-row";
  const kk = document.createElement("span");
  kk.className = "pane-k";
  kk.textContent = k;
  const vv = document.createElement("span");
  vv.className = "pane-v";
  vv.textContent = v;
  vv.title = v;
  row.append(kk, vv);
  return row;
}

function paneButton(glyph: string, label: string, onClick: () => void): HTMLElement {
  const btn = document.createElement("button");
  btn.className = "pane-btn";
  btn.innerHTML = `<span class="fluent">${glyph}</span><span>${label}</span>`;
  btn.onclick = onClick;
  return btn;
}

function renderSidePane() {
  const pane = $("side-pane");
  const resizer = $("pane-resizer");
  if (!settings.detailsPane && !settings.previewPane) {
    pane.style.display = "none";
    resizer.style.display = "none";
    pane.innerHTML = "";
    return;
  }
  pane.style.display = "";
  resizer.style.display = "";
  const tab = activeTab();
  const sel = [...tab.selection];
  // 未选中时显示当前文件夹本身（与资源管理器一致）
  let entry = sel.length === 1 ? tab.listing?.entries.find((e) => e.parse_path === sel[0]) : undefined;
  let isSelf = false;
  if (!entry && sel.length === 0 && tab.listing) {
    isSelf = true;
    entry = {
      name: tab.listing.folder_name,
      full_name: tab.listing.folder_name,
      parse_path: tab.listing.parse_path,
      fs_path: tab.listing.parse_path.startsWith("::") ? null : tab.listing.parse_path,
      is_folder: true, sort_as_folder: true, is_hidden: false,
      size: null, size_text: "", date_modified: 0, date_text: "",
      type_text: "文件夹", ext: "", drive_total: null, drive_free: null, drive_text: "",
    } as ShellEntry;
  }
  pane.innerHTML = "";
  if (!entry) {
    const sub = document.createElement("div");
    sub.className = "pane-sub";
    sub.textContent = `选中 ${sel.length} 个项目`;
    sub.style.padding = "24px";
    pane.append(sub);
    return;
  }
  const e = entry;
  const isImage = !e.is_folder && IMAGE_EXTS.has(e.ext);
  const token = ++sidePaneToken;

  // 顶部 hero 区：图片填充 / 其他显示大图标
  const hero = document.createElement("div");
  hero.className = "pane-hero";
  const img = document.createElement("img");
  img.className = isImage ? "pane-photo" : "pane-bigicon";
  hero.append(img);
  const size = 512;
  const cached = iconCache.get(`t${size}|${e.parse_path}`);
  if (cached) img.src = cached;
  else {
    void invoke<(string | null)[]>("get_thumbnails", { paths: [e.parse_path], size }).then((r) => {
      if (r[0]) {
        iconCache.set(`t${size}|${e.parse_path}`, r[0]);
        if (token === sidePaneToken) img.src = r[0];
      }
    });
  }

  // 预览窗格：仅大图 + 名称
  if (settings.previewPane) {
    hero.classList.add("pane-hero-fill");
    const title = document.createElement("div");
    title.className = "pane-name";
    title.textContent = displayName(e);
    pane.append(hero, title);
    return;
  }

  // 详细信息窗格
  const body = document.createElement("div");
  body.className = "pane-body";

  if (e.is_folder) {
    const nm = document.createElement("div");
    nm.className = "pane-name";
    nm.textContent = displayName(e);
    body.append(nm);
  } else {
    // 文件：小图标 + 名称 一行
    const row = document.createElement("div");
    row.className = "pane-file-row";
    const ic = document.createElement("img");
    ic.src = getIcon(e.parse_path, 32) || "data:image/gif;base64,R0lGODlhAQABAAAAACwAAAAAAQABAAA=";
    const nm = document.createElement("span");
    nm.className = "pane-file-name";
    nm.textContent = displayName(e);
    nm.title = e.name;
    row.append(ic, nm);
    body.append(row);
  }

  const section = document.createElement("div");
  section.className = "pane-section";
  section.textContent = "详细信息";
  body.append(section);

  const rows = document.createElement("div");
  rows.className = "pane-rows";
  // 先显示基础行，随后用 Shell PreviewDetails 完整属性列表替换（与资源管理器一致）
  rows.append(paneRow("类型", e.type_text || "文件夹"));
  if (!e.is_folder && e.size != null) rows.append(paneRow("大小", formatBytes(e.size)));
  const loc = parentDir(e.fs_path);
  if (loc) rows.append(paneRow("文件位置", loc));
  if (e.date_text) rows.append(paneRow("修改日期", e.date_text));
  body.append(rows);

  // 完整属性（按文件类型：图片含分辨率/拍摄日期，音频含艺术家/时长，文档含作者等）
  void invoke<[string, string][]>("get_item_details", { path: e.parse_path }).then((details) => {
    if (token !== sidePaneToken || details.length === 0) return;
    rows.innerHTML = "";
    const hasLoc = details.some(([k]) => k.includes("位置") || k.includes("路径"));
    for (const [k, v] of details) rows.append(paneRow(k, v));
    // 资源管理器额外显示文件位置，若属性列表未含则补充
    if (!hasLoc && loc) rows.append(paneRow("文件位置", loc));
  });

  const target = e.parse_path;
  body.append(paneButton("&#xE90F;", "属性", () => {
    void invoke("invoke_verb", { selection: [target], background: null, verb: "properties" });
  }));
  void isSelf;

  pane.append(hero, body);
}

/* ===================== 下拉菜单（Win11 风格） ===================== */
interface MenuItem {
  label?: string;
  glyph?: string;      // Segoe Fluent 图标
  iconImg?: string;    // 位图图标（data URL，如 ShellNew 菜单）
  accel?: string;      // 右侧快捷键文本（如 "Ctrl+Shift+C"）
  checked?: boolean;   // 选中标记
  checkStyle?: "dot" | "check"; // 圆点（单选）/ 对勾（开关）
  separator?: boolean;
  disabled?: boolean;
  accent?: boolean;    // 图标用强调色（如"更多"菜单）
  submenu?: MenuItem[];
  onClick?: () => void;
}

let dropdownAnchor: HTMLElement | null = null;
let suppressAnchor: HTMLElement | null = null;
// Fluent 右键菜单是否挂起着后端菜单实例
let ctxMenuOpen = false;

// 资源管理器式滑出动画：菜单整体从按钮下方滑下来，收起时整体滑回去
// 实现：外层 .dropdown-wrap 做裁剪，内层菜单 translateY(-100% -> 0)
const DD_EASE = "cubic-bezier(0, 0, 0, 1)";

function wrapMenu(menu: HTMLElement): HTMLElement {
  const wrap = document.createElement("div");
  wrap.className = "dropdown-wrap";
  wrap.append(menu);
  return wrap;
}

function slideIn(wrap: HTMLElement) {
  const menu = wrap.firstElementChild as HTMLElement | null;
  if (!menu) return;
  menu.animate(
    [{ transform: "translateY(-100%)" }, { transform: "translateY(0)" }],
    { duration: 180, easing: DD_EASE },
  );
}

function slideOut(wrap: HTMLElement) {
  if (wrap.dataset.closing) return;
  wrap.dataset.closing = "1";
  wrap.style.pointerEvents = "none";
  const menu = wrap.firstElementChild as HTMLElement | null;
  if (!menu) {
    wrap.remove();
    return;
  }
  const anim = menu.animate(
    [{ transform: "translateY(0)" }, { transform: "translateY(-100%)" }],
    { duration: 150, easing: DD_EASE },
  );
  anim.onfinish = () => wrap.remove();
  anim.oncancel = () => wrap.remove();
  // 兜底：动画回调偶发不触发时强制移除，避免菜单（尤其二级子菜单）永久残留
  window.setTimeout(() => wrap.remove(), 220);
}

// 关闭某菜单及其派生的所有后代子菜单（避免三级及更深子菜单成为孤儿残留）
function slideOutWithChildren(wrap: HTMLElement) {
  document.querySelectorAll<HTMLElement>(".dropdown-wrap").forEach((w) => {
    let p = (w as any).__ownerWrap as HTMLElement | undefined;
    while (p) {
      if (p === wrap) { slideOut(w); break; }
      p = (p as any).__ownerWrap;
    }
  });
  slideOut(wrap);
}

function closeDropdown() {
  document.querySelectorAll<HTMLElement>(".dropdown-wrap").forEach(slideOut);
  dropdownAnchor = null;
  document.removeEventListener("mousedown", onDocDown, true);
  ctxUiOpen = false;
  // Fluent 右键菜单未选择即关闭：释放后端挂起的菜单实例
  if (ctxMenuOpen) {
    ctxMenuOpen = false;
    void invoke("close_ctx_menu");
  }
}
function onDocDown(ev: MouseEvent) {
  const t = ev.target as HTMLElement;
  if (t.closest(".dropdown")) return;
  // 点在触发按钮上：关闭后抑制紧随的 click 重新打开（实现开/关切换）
  const a = dropdownAnchor;
  closeDropdown();
  if (a && (a === t || a.contains(t))) {
    suppressAnchor = a;
    setTimeout(() => { if (suppressAnchor === a) suppressAnchor = null; }, 300);
  }
}

function buildMenu(items: MenuItem[]): HTMLElement {
  const menu = document.createElement("div");
  menu.className = "dropdown";
  // 整组都没有可勾选项/图标时不渲染对应列，避免留白
  const hasCheckColumn = items.some((it) => it.checked !== undefined);
  const hasGlyphColumn = items.some((it) => it.glyph || it.iconImg);
  for (const it of items) {
    if (it.separator) {
      const s = document.createElement("div");
      s.className = "dropdown-sep";
      menu.append(s);
      continue;
    }
    const el = document.createElement("div");
    el.className = "dropdown-item";
    if (it.disabled) el.classList.add("disabled");
    if (hasCheckColumn) {
      const check = document.createElement("span");
      check.className = "fluent dd-check";
      check.innerHTML = it.checked ? (it.checkStyle === "check" ? "&#xE73E;" : "&#xE915;") : "";
      if (it.checked && it.checkStyle !== "check") { check.textContent = "●"; check.style.fontSize = "7px"; }
      el.append(check);
    }
    const glyph = document.createElement("span");
    glyph.className = "fluent dd-glyph" + (it.accent && !it.disabled ? " accent-glyph" : "");
    glyph.innerHTML = it.glyph ?? "";
    if (it.iconImg) {
      // 位图图标优先（ShellNew 系统图标）
      glyph.innerHTML = "";
      const im = document.createElement("img");
      im.className = "dd-img";
      im.src = it.iconImg;
      glyph.append(im);
    }
    const label = document.createElement("span");
    label.className = "dd-label";
    label.textContent = it.label ?? "";
    if (hasGlyphColumn) el.append(glyph);
    el.append(label);
    if (it.accel) {
      const ac = document.createElement("span");
      ac.className = "dd-accel";
      ac.textContent = it.accel;
      el.append(ac);
    }
    if (it.submenu) {
      const arrow = document.createElement("span");
      arrow.className = "fluent dd-arrow";
      arrow.innerHTML = "&#xE76C;";
      el.append(arrow);
      let sub: HTMLElement | null = null;
      const openSub = () => {
        if (sub) return;
        // 所属菜单正在关闭/已移除时不再弹出子菜单，避免关闭后产生孤儿子菜单残留
        const ownerWrap = menu.closest<HTMLElement>(".dropdown-wrap");
        if (!ownerWrap || ownerWrap.dataset.closing || !ownerWrap.isConnected) return;
        sub = wrapMenu(buildMenu(it.submenu!));
        // 记录父链，供级联关闭后代子菜单
        (sub as any).__ownerWrap = ownerWrap;
        const subInner = sub.firstElementChild as HTMLElement;
        // 悬停子菜单（如"显示更多选项"）：尽量用满视口高度，一屏展示更多内容
        subInner.classList.add("submenu-tall");
        document.body.append(sub);
        const inner = sub.firstElementChild as HTMLElement;
        const r = el.getBoundingClientRect();
        const w = inner.offsetWidth;
        const x = r.right + w > window.innerWidth ? r.left - w : r.right;
        const y = Math.max(8, Math.min(r.top, window.innerHeight - inner.offsetHeight - 8));
        // 补偿容器左侧 24px 阴影留白
        sub.style.left = `${x - 24}px`;
        sub.style.top = `${y}px`;
        slideIn(sub);
      };
      el.onmouseenter = () => {
        // 关闭同级其他子菜单
        menu.querySelectorAll(":scope > .dropdown-item").forEach((sib) => {
          if (sib !== el) (sib as any).__closeSub?.();
        });
        openSub();
      };
      (el as any).__closeSub = () => { if (sub) { slideOutWithChildren(sub); sub = null; } };
      el.onclick = (ev) => { ev.stopPropagation(); openSub(); };
    } else {
      el.onmouseenter = () => {
        menu.querySelectorAll(":scope > .dropdown-item").forEach((sib) => (sib as any).__closeSub?.());
      };
      el.onclick = () => {
        if (it.disabled) return;
        // 先执行动作再关菜单：避免 closeDropdown 的后端释放与菜单项 invoke 产生竞态
        it.onClick?.();
        closeDropdown();
      };
    }
    menu.append(el);
  }
  return menu;
}

function showDropdown(anchor: HTMLElement, items: MenuItem[]) {
  // 再次点击同一按钮 = 关闭（切换行为）
  if (suppressAnchor === anchor) {
    suppressAnchor = null;
    return;
  }
  closeDropdown();
  const menu = wrapMenu(buildMenu(items));
  document.body.append(menu);
  const inner = menu.firstElementChild as HTMLElement;
  const r = anchor.getBoundingClientRect();
  const x = Math.min(r.left, window.innerWidth - inner.offsetWidth - 8);
  // 长列表不超出窗口底部（菜单自身 max-height 限制 + 内部滚动）；补偿左侧 24px 阴影留白
  const y = Math.max(8, Math.min(r.bottom + 4, window.innerHeight - inner.offsetHeight - 8));
  menu.style.left = `${x - 24}px`;
  menu.style.top = `${y}px`;
  slideIn(menu);
  dropdownAnchor = anchor;
  setTimeout(() => document.addEventListener("mousedown", onDocDown, true), 0);
}

// 在任意坐标弹出（如表头右键菜单）
function showDropdownAt(px: number, py: number, items: MenuItem[]) {
  closeDropdown();
  const menu = wrapMenu(buildMenu(items));
  document.body.append(menu);
  const inner = menu.firstElementChild as HTMLElement;
  const x = Math.min(px, window.innerWidth - inner.offsetWidth - 8);
  const y = Math.max(8, Math.min(py, window.innerHeight - inner.offsetHeight - 8));
  menu.style.left = `${x - 24}px`;
  menu.style.top = `${y}px`;
  slideIn(menu);
  setTimeout(() => document.addEventListener("mousedown", onDocDown, true), 0);
}

// 表头右键：列可见性开关（名称固定）+ 重置列宽
function showHeaderMenu(x: number, y: number) {
  const toggleCol = (key: SortKey): MenuItem => ({
    label: COLS.find((c) => c.key === key)!.label,
    checked: colVisible[key],
    checkStyle: "check",
    onClick: () => {
      colVisible[key] = !colVisible[key];
      localStorage.setItem("colVisible", JSON.stringify(colVisible));
      renderHeader();
      renderList();
    },
  });
  showDropdownAt(x, y, [
    {
      label: "将所有列调整为合适的大小(A)",
      onClick: () => {
        Object.assign(colWidths, COL_DEFAULT_W);
        localStorage.setItem("colWidths", JSON.stringify(colWidths));
        renderHeader();
        renderList();
      },
    },
    { separator: true },
    { label: "名称", checked: true, checkStyle: "check", disabled: true },
    toggleCol("date"),
    toggleCol("created"),
    toggleCol("type"),
    toggleCol("size"),
  ]);
}

function showSortMenu(anchor: HTMLElement) {
  const tab = activeTab();
  const keyItem = (key: Tab["sortKey"], label: string): MenuItem => ({
    label,
    checked: tab.sortKey === key,
    onClick: () => { tab.sortKey = key; renderHeader(); renderList(); },
  });
  showDropdown(anchor, [
    keyItem("name", "名称"),
    keyItem("date", "修改日期"),
    keyItem("type", "类型"),
    keyItem("size", "大小"),
    { separator: true },
    { label: "递增", checked: tab.sortAsc, onClick: () => { tab.sortAsc = true; renderHeader(); renderList(); } },
    { label: "递减", checked: !tab.sortAsc, onClick: () => { tab.sortAsc = false; renderHeader(); renderList(); } },
  ]);
}

function setView(view: ViewMode) {
  const tab = activeTab();
  tab.view = view;
  renderHeader();
  renderList();
  renderViewButtons();
  // 写回 ShellBag，与资源管理器共享该文件夹的视图设置
  const p = tab.listing?.parse_path ?? tab.history[tab.historyIndex];
  if (p) void invoke("set_view_mode", { path: p, view });
}

function renderViewButtons() {
  const btns = document.querySelectorAll<HTMLElement>(".statusbar .status-btn");
  const view = activeTab().view;
  btns[0]?.classList.toggle("active", view === "details");
  btns[1]?.classList.toggle("active", view !== "details");
}

// 查看菜单：与资源管理器一致（8 种视图 + 窗格 + 显示子菜单）
function showViewMenu(anchor: HTMLElement) {
  const tab = activeTab();
  const vi = (view: ViewMode, label: string, glyph: string): MenuItem => ({
    label,
    glyph,
    checked: tab.view === view,
    checkStyle: "dot",
    onClick: () => setView(view),
  });
  const toggle = (key: keyof typeof settings, label: string, glyph: string, after?: () => void): MenuItem => ({
    label,
    glyph,
    checked: settings[key],
    checkStyle: "check",
    onClick: () => {
      settings[key] = !settings[key];
      renderAll();
      after?.();
    },
  });
  showDropdown(anchor, [
    vi("xl-icons", "超大图标", "&#xE71D;"),
    vi("l-icons", "大图标", "&#xE922;"),
    vi("m-icons", "中图标", "&#xF0E2;"),
    vi("s-icons", "小图标", "&#xE80A;"),
    vi("list", "列表", "&#xEA37;"),
    vi("details", "详细信息", "&#xE8FD;"),
    vi("tiles", "平铺", "&#xE8A9;"),
    vi("content", "内容", "&#xE8A4;"),
    { separator: true },
    {
      label: "详细信息窗格", glyph: "&#xE8A0;", checked: settings.detailsPane, checkStyle: "dot",
      onClick: () => { settings.detailsPane = !settings.detailsPane; if (settings.detailsPane) settings.previewPane = false; renderAll(); },
    },
    {
      label: "预览窗格", glyph: "&#xE8A1;", checked: settings.previewPane, checkStyle: "dot",
      onClick: () => { settings.previewPane = !settings.previewPane; if (settings.previewPane) settings.detailsPane = false; renderAll(); },
    },
    { separator: true },
    {
      label: "显示",
      submenu: [
        toggle("navPane", "导航窗格", "&#xE8A0;"),
        toggle("compact", "紧凑视图", "&#xE8FD;"),
        { separator: true },
        toggle("checkboxes", "项目复选框", "&#xE73A;"),
        toggle("showExt", "文件扩展名", "&#xE7C3;"),
        toggle("showHidden", "隐藏的项目", "&#xE7B3;"),
      ],
    },
  ]);
}

// "新建"菜单：通过 Shell COM 获取系统 ShellNew 模板列表（与资源管理器完全一致）
interface NewMenuEntry { id: number; label: string; icon: string | null; separator: boolean; }

async function showNewMenu(anchor: HTMLElement) {
  const folder = activeTab().listing?.parse_path;
  if (!folder) return;
  let entries: NewMenuEntry[] = [];
  try {
    entries = await invoke<NewMenuEntry[]>("get_new_menu", { folder });
  } catch (e) { console.error(e); }
  if (entries.length === 0) {
    // 回退：虚拟位置（如此电脑）无背景新建菜单
    showDropdown(anchor, [
      { label: "文件夹", glyph: "&#xE8B7;", onClick: () => void createNewFolder() },
    ]);
    return;
  }
  let fileIdx = 0;
  showDropdown(anchor, entries.map((e) => {
    if (e.separator) return { separator: true } as MenuItem;
    const i = fileIdx++;
    return {
      label: e.label,
      iconImg: e.icon ?? undefined,
      glyph: e.icon ? undefined : (i === 0 ? "&#xE8B7;" : "&#xE7C3;"),
      onClick: () => {
        void invoke("invoke_new_item", { id: e.id }).then(() => {
          setTimeout(() => void refresh(), 500);
          setTimeout(() => void refresh(), 1500);
        });
      },
    } as MenuItem;
  }));
}

/* ===================== "更多"菜单（···） ===================== */
// 撤销栈：只记录应用内可逆的操作（重命名/新建文件夹），有内容时"撤消"才显示
interface UndoOp {
  label: string;
  run: () => Promise<void>;
}
const undoStack: UndoOp[] = [];

async function doUndo() {
  const op = undoStack.pop();
  if (!op) return;
  try {
    await op.run();
  } catch (e) {
    console.error("undo failed:", e);
  }
  void refresh();
}

function selectAllItems() {
  const tab = activeTab();
  tab.selection = new Set(sortedEntries(tab).map((e) => e.parse_path));
  renderList();
  renderStatus();
}
function clearAllSelection() {
  const tab = activeTab();
  tab.selection.clear();
  tab.anchorIndex = -1;
  renderList();
  renderStatus();
}
function invertSelection() {
  const tab = activeTab();
  const all = sortedEntries(tab).map((e) => e.parse_path);
  tab.selection = new Set(all.filter((p) => !tab.selection.has(p)));
  renderList();
  renderStatus();
}
function showProperties() {
  const tab = activeTab();
  const sel = [...tab.selection];
  const target = sel.length > 0 ? sel : tab.listing ? [tab.listing.parse_path] : [];
  if (target.length > 0) {
    void invoke("invoke_verb", { selection: target, background: null, verb: "properties" });
  }
}

// 驱动器工具组（清理/优化/格式化）：此电脑与磁盘根目录的"更多"菜单共用
function driveToolItems(drive: string | null): MenuItem[] {
  return [
    {
      label: "清理", glyph: "&#xEA99;", accent: true,
      onClick: () => void invoke("system_action", { action: drive ? `clean-drive:${drive}` : "clean-drive" }),
    },
    {
      label: "优化", glyph: "&#xEC4A;", accent: true,
      onClick: () => void invoke("system_action", { action: "optimize-drives" }),
    },
    {
      label: "格式化", glyph: "&#xE977;", accent: true, disabled: !drive,
      onClick: () => void invoke("format_drive", { letter: drive }),
    },
  ];
}

function showMoreMenu(anchor: HTMLElement) {
  const tab = activeTab();
  const items: MenuItem[] = [];
  // 有可撤消操作时才显示"撤消"（与需求一致）
  const top = undoStack[undoStack.length - 1];
  if (top) {
    items.push(
      { label: `撤消${top.label}`, glyph: "&#xE7A7;", accent: true, onClick: () => void doUndo() },
      { separator: true },
    );
  }
  // 此电脑视图：驱动器工具组（清理/优化/格式化，与资源管理器一致）
  const cur = tab.listing?.parse_path ?? "";
  const isDriveRoot = /^[A-Za-z]:\\$/.test(cur);
  // 固定到快速访问（磁盘根/普通文件夹/选中项共用）
  const pinItem = (path: string): MenuItem => ({
    label: "固定到快速访问", glyph: "&#xE718;", accent: true,
    onClick: () => {
      void invoke("invoke_verb", { selection: [path], background: null, verb: "pintohome" }).then(() => void loadSidebar());
    },
  });
  // 选中了真实文件系统项（非虚拟位置，非此电脑磁盘列表）：压缩 ZIP / 固定 / 复制路径
  const sel = [...tab.selection];
  const hasFsSelection = cur !== THIS_PC && sel.length > 0
    && sel.every((p) => !p.startsWith("::") && !p.startsWith("shell:"));
  if (hasFsSelection) {
    // 选中均为文件 → 添加到收藏夹；含文件夹 → 固定到快速访问（与资源管理器一致）
    const selEntries = sel
      .map((p) => tab.listing?.entries.find((e) => e.parse_path === p))
      .filter((e): e is ShellEntry => !!e);
    const allFiles = selEntries.length > 0 && selEntries.every((e) => !e.is_folder);
    const pinOrFav: MenuItem = allFiles
      ? {
          label: "添加到收藏夹", glyph: "&#xE734;", accent: true,
          onClick: () => void invoke("add_to_favorites", { selection: sel }).then(() => void loadSidebar()),
        }
      : {
          label: "固定到快速访问", glyph: "&#xE718;", accent: true,
          onClick: () => {
            void invoke("invoke_verb", { selection: sel, background: null, verb: "pintohome" }).then(() => void loadSidebar());
          },
        };
    items.push(
      {
        label: "压缩为 ZIP 文件", glyph: "&#xE8B5;", accent: true,
        onClick: () => void invoke("compress_to_zip", { selection: sel }).then(() => {
          setTimeout(() => void refresh(), 600);
          setTimeout(() => void refresh(), 1800);
        }),
      },
      pinOrFav,
      { label: "复制路径", glyph: "&#xE8C8;", accent: true, onClick: copyAddresses },
      { separator: true },
    );
  } else if (cur === THIS_PC) {
    // 仅当选中单个本地磁盘时才显示驱动器工具组（无选中不显示，与资源管理器一致）
    const drive = sel.length === 1 && /^[A-Za-z]:\\$/.test(sel[0]) ? sel[0][0] : null;
    if (drive) items.push(...driveToolItems(drive), { separator: true });
  } else if (isDriveRoot) {
    // 磁盘根目录：工具组针对当前盘 + 固定到快速访问（与资源管理器一致）
    items.push(
      ...driveToolItems(cur[0]),
      { separator: true },
      pinItem(cur),
      { separator: true },
    );
  } else if (cur && !cur.startsWith("::") && !cur.startsWith("shell:")) {
    // 普通文件夹（含网络/WSL 路径）：仅固定到快速访问（与资源管理器一致）
    items.push(pinItem(cur), { separator: true });
  }
  // 网络组：仅无选中的此电脑/网络视图显示（与资源管理器一致）
  if (!hasFsSelection && (cur === THIS_PC || cur === "::{F02C1A0D-BE21-4350-88B0-7367FC96EF3C}")) {
    items.push(
      {
        label: "连接到媒体服务器", glyph: "&#xE953;", accent: true,
        onClick: () => void invoke("system_action", { action: "add-media-server" }),
      },
      {
        label: "添加一个网络位置", glyph: "&#xE969;", accent: true,
        onClick: () => void invoke("system_action", { action: "add-network-place" }),
      },
      {
        label: "映射网络驱动器", glyph: "&#xE8CE;", accent: true,
        onClick: () => void invoke("system_action", { action: "map-drive" }),
      },
      {
        label: "断开网络驱动器的连接", glyph: "&#xE8CD;", accent: true,
        onClick: () => void invoke("system_action", { action: "disconnect-drive" }),
      },
      { separator: true },
    );
  }
  items.push(
    { label: "全部选择", glyph: "&#xE8B3;", accent: true, onClick: selectAllItems },
    { label: "全部取消", glyph: "&#xE8E6;", accent: true, onClick: clearAllSelection },
    { label: "反向选择", glyph: "&#xE746;", accent: true, onClick: invertSelection },
    { separator: true },
    { label: "属性", glyph: "&#xE90F;", accent: true, onClick: showProperties },
    {
      label: "选项", glyph: "&#xE713;", accent: true,
      onClick: () => void invoke("system_action", { action: "folder-options" }),
    },
  );
  showDropdown(anchor, items);
}

/* ===================== 地址栏编辑 ===================== */
// 规范化手输路径：去引号/空白，全角转半角，正斜杠转反斜杠，盘符根补尾斜杠
function normalizePath(raw: string): string {
  let p = raw.trim().replace(/^"|"$/g, "");
  p = p.replace(/：/g, ":").replace(/[＼／\/]/g, "\\");
  if (/^[a-zA-Z]:$/.test(p)) p += "\\";
  return p;
}

function startAddressEdit() {
  const box = $("breadcrumb-box");
  if (box.querySelector(".address-input")) return;
  const listing = activeTab().listing;
  const bc = $("breadcrumb");
  bc.style.display = "none";
  const input = document.createElement("input");
  input.className = "address-input";
  const listing2 = listing;
  input.value = listing2 ? (listing2.parse_path.startsWith("::") ? listing2.folder_name : listing2.parse_path) : "";
  input.spellcheck = false;
  box.append(input);
  input.focus();
  input.select();
  // finish 必须幂等：remove() 会同步触发 blur，否则重入报 NotFoundError 中断 Enter 处理
  let closed = false;
  const finish = () => {
    if (closed) return;
    closed = true;
    input.onblur = null;
    input.remove();
    bc.style.display = "";
  };
  input.onblur = finish;
  input.onkeydown = (ev) => {
    ev.stopPropagation();
    if (ev.key === "Escape") finish();
    if (ev.key === "Enter") {
      const v = normalizePath(input.value);
      finish();
      if (v) {
        void navigate(v).then((ok) => {
          if (!ok) {
            // 与资源管理器一致：路径无效时给出提示
            alert(`Windows 找不到"${v}"。请检查拼写并重试。`);
          }
        });
      }
    }
  };
}

/* ===================== 交互动作 ===================== */
// Fluent 右键菜单（选中项）：后端经典菜单树 + 前端 Win11 风格渲染
interface CtxNode {
  id: number;
  label: string;
  accel: string;
  verb: string;
  icon: string | null;
  separator: boolean;
  children: CtxNode[];
}

// Win11 现代注册扩展（IExplorerCommand）
interface ModernNode {
  mid: number;
  label: string;
  icon: string | null;
  children: ModernNode[];
}

// 已在快捷条/标准段呈现的经典 verb，扩展段不重复显示
const STD_CTX_VERBS = new Set([
  "open", "openas", "opennewwindow", "opennewtab", "opennewprocess", "explore", "find",
  "cut", "copy", "paste", "delete", "rename", "properties", "link",
  "pintohome", "pintostartscreen", "runas", "edit", "print", "share",
]);

// Win11 现代菜单会直接展示的已注册扩展（IExplorerCommand 类）；
// 经典枚举无法区分注册方式，按已知现代扩展名称白名单过滤，其余全部收进"显示更多选项"
const MODERN_EXT_PATTERNS = [
  /nanazip/i,
  /onedrive/i,
  /file locksmith/i,
  /powerrename/i,
  /\bcode\b/i,
  /\bzed\b/i,
  /终端|terminal/i,
  /压缩到|compress/i,
];
const isModernExt = (label: string) => MODERN_EXT_PATTERNS.some((re) => re.test(label));

let ctxToken = 0;
let ctxUiOpen = false;

// 复制所选项的文件地址到剪贴板
function copyAddresses() {
  const tab = activeTab();
  const paths = [...tab.selection].map(
    (p) => tab.listing?.entries.find((e) => e.parse_path === p)?.fs_path ?? p,
  );
  if (paths.length) writeClipboard(paths.join("\n"));
}

// 剪贴板写入：直接走后端 Win32（前端 navigator.clipboard 在 Tauri webview
// 非安全上下文下不可靠/缺失，后端实现总能生效）
function writeClipboard(text: string) {
  void invoke("set_clipboard_text", { text });
}

// 顶部快捷条：剪切/复制/重命名/删除（与原版一致）
function buildCtxQuickbar(sel: string[]): HTMLElement {
  const bar = document.createElement("div");
  bar.className = "ctx-quickbar";
  const virt = isVirtualLocation();
  const btn = (glyph: string, label: string, onClick: () => void, disabled: boolean) => {
    const b = document.createElement("button");
    b.className = "ctx-qbtn";
    b.disabled = disabled;
    b.innerHTML = `<span class="fluent">${glyph}</span><span>${label}</span>`;
    b.onclick = () => { onClick(); closeDropdown(); };
    bar.append(b);
  };
  btn("&#xE8C6;", "剪切", () => void runVerb("cut"), virt);
  btn("&#xE8C8;", "复制", () => void runVerb("copy"), virt);
  btn("&#xE8AC;", "重命名", () => startRename(), virt || sel.length !== 1);
  btn("&#xE74D;", "删除", () => void runVerb("delete"), virt);
  return bar;
}

// 在鼠标坐标附近弹出带快捷条的 Fluent 右键菜单；一级菜单不滚动，
// 放不下时整体上移完整显示（位置可脱离鼠标）；quickbar 传 null 表示不显示快捷条
function showCtxMenuAt(x: number, y: number, items: MenuItem[], sel: string[], quickbar?: HTMLElement | null) {
  closeDropdown();
  const menu = wrapMenu(buildMenu(items));
  const inner = menu.firstElementChild as HTMLElement;
  inner.classList.add("ctx-menu");
  if (quickbar !== null) inner.prepend(quickbar ?? buildCtxQuickbar(sel));
  document.body.append(menu);
  // 极端情况（菜单比视口还高）才回退到内部滚动
  if (inner.offsetHeight > window.innerHeight - 16) {
    inner.style.maxHeight = `${window.innerHeight - 16}px`;
    inner.style.overflowY = "auto";
  }
  const px = Math.min(x, window.innerWidth - inner.offsetWidth - 8);
  const py = Math.max(8, Math.min(y, window.innerHeight - inner.offsetHeight - 8));
  menu.style.left = `${px - 24}px`;
  menu.style.top = `${py}px`;
  slideIn(menu);
  setTimeout(() => document.addEventListener("mousedown", onDocDown, true), 0);
}

async function openEntry(e: ShellEntry) {
  if (e.is_folder) {
    void navigate(e.parse_path);
  } else {
    await invoke("open_item", { path: e.parse_path });
  }
}

async function showItemMenu(x: number, y: number) {
  const tab = activeTab();
  const sel = [...tab.selection];
  if (sel.length === 0) return;
  const entry = sel.length === 1 ? tab.listing?.entries.find((e) => e.parse_path === sel[0]) : undefined;
  // 磁盘（驱动器根）：菜单结构与资源管理器驱动器菜单一致
  const isDriveEntry = !!entry && /^[A-Za-z]:\\$/.test(entry.parse_path);
  const token = ++ctxToken;

  const stdItems = (extra: MenuItem[], mid: MenuItem[] = []): MenuItem[] => {
    const items: MenuItem[] = [];
    if (entry) {
      items.push({ label: "打开", glyph: "&#xE8E5;", accel: "Enter", onClick: () => void openEntry(entry) });
      if (entry.is_folder) {
        items.push({ label: "在新标签页中打开", glyph: "&#xE8AD;", onClick: () => addTab(entry.parse_path) });
        items.push({
          label: "固定到快速访问", glyph: "&#xE718;",
          onClick: () => {
            void invoke("invoke_verb", { selection: sel, background: null, verb: "pintohome" }).then(() => void loadSidebar());
          },
        });
      }
    }
    items.push(...mid);
    items.push({ label: "复制文件地址", glyph: "&#xE8C8;", accel: "Ctrl+Shift+C", onClick: copyAddresses });
    items.push({ label: "属性", glyph: "&#xE90F;", accel: "Alt+Enter", onClick: showProperties });
    items.push(...extra);
    return items;
  };

  // 加载完成后一次性显示菜单（经典树 + 现代扩展并行获取，后端已做实例/图标缓存）
  let tree: CtxNode[] = [];
  let modern: ModernNode[] = [];
  let canPaste = false;
  try {
    [tree, modern, canPaste] = await Promise.all([
      invoke<CtxNode[]>("get_ctx_menu", { selection: sel }).catch(() => [] as CtxNode[]),
      invoke<ModernNode[]>("get_modern_menu", { selection: sel }).catch(() => [] as ModernNode[]),
      isDriveEntry ? invoke<boolean>("clipboard_has_files").catch(() => false) : Promise.resolve(false),
    ]);
  } catch (e) { console.error(e); }
  // 期间又发起了新菜单：释放后端实例
  if (token !== ctxToken) {
    if (tree.length) void invoke("close_ctx_menu");
    return;
  }

  const nodeToItem = (n: CtxNode): MenuItem => {
    if (n.separator) return { separator: true };
    const clickable = n.children.length === 0 && n.id >= 1 && n.id <= 0x7fff;
    return {
      label: n.label,
      accel: n.accel || undefined,
      iconImg: n.icon ?? undefined,
      submenu: n.children.length ? n.children.map(nodeToItem) : undefined,
      disabled: !clickable && n.children.length === 0,
      onClick: clickable
        ? () => {
            ctxMenuOpen = false;
            void invoke<MenuResult>("invoke_ctx_item", { id: n.id }).then((r) => handleMenuResult(r, sel));
          }
        : undefined,
    };
  };

  // 扩展段：优先用真实现代注册扩展（IExplorerCommand 枚举，与资源管理器一级菜单同源）；
  // 枚举不可用时回退到名称白名单过滤经典树
  const modernToItem = (n: ModernNode): MenuItem => ({
    label: n.label,
    iconImg: n.icon ?? undefined,
    submenu: n.children.length ? n.children.map(modernToItem) : undefined,
    onClick: n.children.length ? undefined : () => {
      ctxMenuOpen = false;
      void invoke("invoke_modern_item", { mid: n.mid }).then(() => {
        void invoke("close_ctx_menu");
        setTimeout(() => void refresh(), 600);
        setTimeout(() => void refresh(), 1800);
      });
    },
  });
  let ext: MenuItem[];
  if (modern.length) {
    ext = modern.map(modernToItem);
  } else {
    ext = [];
    for (const n of tree) {
      if (n.separator) continue;
      if (n.verb && STD_CTX_VERBS.has(n.verb.toLowerCase())) continue;
      if (!isModernExt(n.label)) continue;
      ext.push(nodeToItem(n));
    }
  }
  // 固定到"开始"：经典树里找到对应 verb 后作为标准段项呈现
  const mid: MenuItem[] = [];
  const pinStart = tree.find((n) => n.verb.toLowerCase() === "pintostartscreen");
  if (pinStart) {
    mid.push({
      label: "固定到“开始”", glyph: "&#xE718;",
      onClick: () => {
        ctxMenuOpen = false;
        void invoke<MenuResult>("invoke_ctx_item", { id: pinStart.id }).then((r) => handleMenuResult(r, sel));
      },
    });
  }
  const extra: MenuItem[] = [];
  if (ext.length) extra.push({ separator: true }, ...ext);
  if (tree.length) {
    extra.push({ separator: true }, {
      label: "显示更多选项", glyph: "&#xE712;",
      submenu: tree.map(nodeToItem),
    });
  }
  if (entry && isDriveEntry) {
    // 驱动器菜单：打开/新标签页/新窗口/格式化/固定到快速访问/固定到开始/属性（与资源管理器一致）
    const driveItems: MenuItem[] = [
      { label: "打开", glyph: "&#xE8E5;", accel: "Enter", onClick: () => void openEntry(entry) },
      { label: "在新标签页中打开", glyph: "&#xE8AD;", onClick: () => addTab(entry.parse_path) },
      { label: "在新窗口中打开", glyph: "&#xE8A7;", onClick: () => void invoke("open_new_window", { path: entry.parse_path }) },
      { label: "格式化...", glyph: "&#xE977;", onClick: () => void invoke("format_drive", { letter: entry.parse_path[0] }) },
      {
        label: "固定到快速访问", glyph: "&#xE718;",
        onClick: () => {
          void invoke("invoke_verb", { selection: sel, background: null, verb: "pintohome" }).then(() => void loadSidebar());
        },
      },
      ...mid, // 固定到"开始"（经典树 verb）
      { label: "属性", glyph: "&#xE90F;", accel: "Alt+Enter", onClick: showProperties },
      ...extra,
    ];
    showCtxMenuAt(x, y, driveItems, sel, buildDriveQuickbar(entry, canPaste));
  } else {
    showCtxMenuAt(x, y, stdItems(extra, mid), sel);
  }
  ctxUiOpen = true;
  ctxMenuOpen = tree.length > 0;
}

// 磁盘卡片版快捷条：复制/粘贴/重命名（与资源管理器驱动器菜单一致）
function buildDriveQuickbar(entry: ShellEntry, canPaste: boolean): HTMLElement {
  const bar = document.createElement("div");
  bar.className = "ctx-quickbar";
  const btn = (glyph: string, label: string, onClick: () => void, disabled = false) => {
    const b = document.createElement("button");
    b.className = "ctx-qbtn";
    b.disabled = disabled;
    b.innerHTML = `<span class="fluent">${glyph}</span><span>${label}</span>`;
    b.onclick = () => { onClick(); closeDropdown(); };
    bar.append(b);
  };
  btn("&#xE8C8;", "复制", () => void invoke("invoke_verb", { selection: [entry.parse_path], background: null, verb: "copy" }));
  // 粘贴：粘进驱动器根目录；剪贴板无文件时置灰
  btn("&#xE77F;", "粘贴", () => void invoke("invoke_verb", { selection: [], background: entry.parse_path, verb: "paste" }), !canPaste);
  btn("&#xE8AC;", "重命名", () => startDriveRename(entry));
  return bar;
}

// 磁盘卡片内联重命名（修改卷标，与资源管理器一致）
function startDriveRename(entry: ShellEntry) {
  const card = document.querySelector<HTMLElement>(`.drive-card[data-path="${CSS.escape(entry.parse_path)}"]`);
  const label = card?.querySelector<HTMLElement>(".drive-name");
  if (!label) return;
  // 去掉尾部盘符后缀，只编辑卷标部分
  const orig = entry.name.replace(/\s*\([A-Za-z]:\)$/, "");
  const input = document.createElement("input");
  input.className = "rename-input";
  input.value = orig;
  label.replaceWith(input);
  input.focus();
  input.select();
  let done = false;
  const commit = async () => {
    if (done) return;
    done = true;
    const newName = input.value.trim();
    if (newName && newName !== orig) {
      try {
        await invoke("rename_item", { path: entry.parse_path, newName });
      } catch (e) { console.error(e); }
    }
    void refresh();
    void loadSidebar();
  };
  input.onblur = () => void commit();
  input.onkeydown = (ev) => {
    ev.stopPropagation();
    if (ev.key === "Enter") void commit();
    if (ev.key === "Escape") { done = true; void refresh(); }
  };
  input.onclick = (ev) => ev.stopPropagation();
}

/* ===================== 侧栏 Fluent 右键菜单（与文件列表完全一致的样式） ===================== */

// 侧栏项内联重命名（与资源管理器导航窗格一致）
function startSideRename(el: HTMLElement, entry: ShellEntry) {
  const label = el.querySelector<HTMLElement>(".side-label");
  if (!label) return;
  const input = document.createElement("input");
  input.className = "rename-input";
  input.value = entry.name;
  label.replaceWith(input);
  input.focus();
  input.select();
  let done = false;
  const commit = async () => {
    if (done) return;
    done = true;
    const newName = input.value.trim();
    if (newName && newName !== entry.name) {
      try {
        await invoke("rename_item", { path: entry.parse_path, newName });
      } catch (e) { console.error(e); }
    }
    void loadSidebar();
    void refresh();
  };
  input.onblur = () => void commit();
  input.onkeydown = (ev) => {
    ev.stopPropagation();
    if (ev.key === "Enter") void commit();
    if (ev.key === "Escape") { done = true; void loadSidebar(); }
  };
  input.onclick = (ev) => ev.stopPropagation();
}

// 侧栏操作后延迟刷新侧栏与当前列表（shell 操作是异步落盘的）
function sideRefreshLater() {
  setTimeout(() => { void loadSidebar(); void refresh(); }, 600);
  setTimeout(() => { void loadSidebar(); void refresh(); }, 1800);
}

// 侧栏版顶部快捷条：直接对侧栏项执行 verb（虚拟节点/驱动器禁用相应操作）
function buildSideQuickbar(entry: ShellEntry, el: HTMLElement): HTMLElement {
  const bar = document.createElement("div");
  bar.className = "ctx-quickbar";
  const sel = [entry.parse_path];
  const virt = !entry.fs_path;
  const isDrive = /^[A-Za-z]:\\$/.test(entry.parse_path);
  const btn = (glyph: string, label: string, onClick: () => void, disabled: boolean) => {
    const b = document.createElement("button");
    b.className = "ctx-qbtn";
    b.disabled = disabled;
    b.innerHTML = `<span class="fluent">${glyph}</span><span>${label}</span>`;
    b.onclick = () => { onClick(); closeDropdown(); };
    bar.append(b);
  };
  const verb = (v: string) => {
    void invoke("invoke_verb", { selection: sel, background: null, verb: v }).then(() => {
      if (v === "cut") { cutPaths = new Set(sel); renderList(); return; }
      if (v === "copy") return;
      cutPaths = new Set();
      sideRefreshLater();
    });
  };
  btn("&#xE8C6;", "剪切", () => verb("cut"), virt || isDrive);
  btn("&#xE8C8;", "复制", () => verb("copy"), virt);
  btn("&#xE8AC;", "重命名", () => startSideRename(el, entry), virt || isDrive);
  btn("&#xE74D;", "删除", () => verb("delete"), virt || isDrive);
  return bar;
}

// 侧栏项 Fluent 右键菜单：结构与文件列表一致（标准段 + 现代扩展段 + 显示更多选项）
async function showSideItemMenu(x: number, y: number, entry: ShellEntry, el: HTMLElement) {
  const sel = [entry.parse_path];
  const token = ++ctxToken;
  let tree: CtxNode[] = [];
  let modern: ModernNode[] = [];
  try {
    [tree, modern] = await Promise.all([
      invoke<CtxNode[]>("get_ctx_menu", { selection: sel }).catch(() => [] as CtxNode[]),
      invoke<ModernNode[]>("get_modern_menu", { selection: sel }).catch(() => [] as ModernNode[]),
    ]);
  } catch (e) { console.error(e); }
  if (token !== ctxToken) {
    if (tree.length) void invoke("close_ctx_menu");
    return;
  }

  const onResult = (r: MenuResult) => {
    if (r.action === "navigate") { void navigate(entry.parse_path); return; }
    if (r.action === "rename") { startSideRename(el, entry); return; }
    if (r.action === "invoked") {
      if (r.verb === "cut") { cutPaths = new Set(sel); renderList(); return; }
      if (r.verb === "copy") return;
      cutPaths = new Set();
      sideRefreshLater();
    }
  };
  const nodeToItem = (n: CtxNode): MenuItem => {
    if (n.separator) return { separator: true };
    const clickable = n.children.length === 0 && n.id >= 1 && n.id <= 0x7fff;
    return {
      label: n.label,
      accel: n.accel || undefined,
      iconImg: n.icon ?? undefined,
      submenu: n.children.length ? n.children.map(nodeToItem) : undefined,
      disabled: !clickable && n.children.length === 0,
      onClick: clickable
        ? () => {
            ctxMenuOpen = false;
            void invoke<MenuResult>("invoke_ctx_item", { id: n.id }).then(onResult);
          }
        : undefined,
    };
  };
  const modernToItem = (n: ModernNode): MenuItem => ({
    label: n.label,
    iconImg: n.icon ?? undefined,
    submenu: n.children.length ? n.children.map(modernToItem) : undefined,
    onClick: n.children.length ? undefined : () => {
      ctxMenuOpen = false;
      void invoke("invoke_modern_item", { mid: n.mid }).then(() => {
        void invoke("close_ctx_menu");
        sideRefreshLater();
      });
    },
  });

  let ext: MenuItem[];
  if (modern.length) {
    ext = modern.map(modernToItem);
  } else {
    ext = [];
    for (const n of tree) {
      if (n.separator) continue;
      if (n.verb && STD_CTX_VERBS.has(n.verb.toLowerCase())) continue;
      if (!isModernExt(n.label)) continue;
      ext.push(nodeToItem(n));
    }
  }

  const items: MenuItem[] = [
    { label: "打开", glyph: "&#xE8E5;", accel: "Enter", onClick: () => void navigate(entry.parse_path) },
    { label: "在新标签页中打开", glyph: "&#xE8AD;", onClick: () => addTab(entry.parse_path) },
  ];
  // 固定/取消固定：与资源管理器同款 shell verb，完成后刷新侧栏
  const pinVerb = (v: string) => {
    void invoke("invoke_verb", { selection: sel, background: null, verb: v }).then(() => void loadSidebar());
  };
  const canPin = tree.some((n) => n.verb.toLowerCase() === "pintohome");
  if (entry.pinned) {
    items.push({
      label: "从快速访问取消固定", glyph: "&#xE77A;",
      onClick: () => {
        void invoke("quick_access_verb", { path: entry.parse_path, verb: "unpinfromhome" }).then(() => {
          void loadSidebar();
          setTimeout(() => void loadSidebar(), 800);
        });
      },
    });
  } else if (canPin) {
    items.push({ label: "固定到快速访问", glyph: "&#xE718;", onClick: () => pinVerb("pintohome") });
  }
  const pinStart = tree.find((n) => n.verb.toLowerCase() === "pintostartscreen");
  if (pinStart) {
    items.push({
      label: "固定到“开始”", glyph: "&#xE718;",
      onClick: () => {
        ctxMenuOpen = false;
        void invoke<MenuResult>("invoke_ctx_item", { id: pinStart.id }).then(onResult);
      },
    });
  }
  if (entry.fs_path) {
    items.push({
      label: "复制文件地址", glyph: "&#xE8C8;", accel: "Ctrl+Shift+C",
      onClick: () => writeClipboard(entry.fs_path!),
    });
  }
  items.push({
    label: "属性", glyph: "&#xE90F;", accel: "Alt+Enter",
    onClick: () => void invoke("invoke_verb", { selection: sel, background: null, verb: "properties" }),
  });
  if (ext.length) items.push({ separator: true }, ...ext);
  if (tree.length) {
    items.push({ separator: true }, {
      label: "显示更多选项", glyph: "&#xE712;",
      submenu: tree.map(nodeToItem),
    });
  }

  showCtxMenuAt(x, y, items, sel, buildSideQuickbar(entry, el));
  ctxUiOpen = true;
  ctxMenuOpen = tree.length > 0;
}

// 背景菜单版顶部快捷条：仅"粘贴"靠左显示（与 Win11 资源管理器空白处菜单一致）；
// 无可粘贴对象时由调用方直接不显示整条快捷条
function buildBgQuickbar(): HTMLElement {
  const bar = document.createElement("div");
  bar.className = "ctx-quickbar single";
  const b = document.createElement("button");
  b.className = "ctx-qbtn";
  b.innerHTML = `<span class="fluent">&#xE77F;</span><span>粘贴</span>`;
  b.onclick = () => { void runVerb("paste"); closeDropdown(); };
  bar.append(b);
  return bar;
}

// 空白处 Fluent 右键菜单：结构与 Win11 资源管理器完全一致
// （粘贴快捷条 + 查看/排序方式/刷新 + 撤消 + 新建 + 属性 + 扩展段 + 显示更多选项）
async function showBackgroundMenu(x: number, y: number) {
  const tab = activeTab();
  const path = tab.listing?.parse_path;
  if (!path) return;
  const token = ++ctxToken;
  const virt = isVirtualLocation();

  // 并行拉取：经典背景菜单树 + 新建模板 + 粘贴可用性（加载完成后一次性显示）
  let tree: CtxNode[] = [];
  let newEntries: NewMenuEntry[] = [];
  let canPaste = false;
  try {
    [tree, newEntries, canPaste] = await Promise.all([
      invoke<CtxNode[]>("get_ctx_menu", { selection: [], background: path }).catch(() => [] as CtxNode[]),
      invoke<NewMenuEntry[]>("get_new_menu", { folder: path }).catch(() => [] as NewMenuEntry[]),
      invoke<boolean>("clipboard_has_files").catch(() => false),
    ]);
  } catch (e) { console.error(e); }
  // 期间又发起了新菜单：释放后端实例
  if (token !== ctxToken) {
    if (tree.length) void invoke("close_ctx_menu");
    return;
  }

  const nodeToItem = (n: CtxNode): MenuItem => {
    if (n.separator) return { separator: true };
    const clickable = n.children.length === 0 && n.id >= 1 && n.id <= 0x7fff;
    return {
      label: n.label,
      accel: n.accel || undefined,
      iconImg: n.icon ?? undefined,
      submenu: n.children.length ? n.children.map(nodeToItem) : undefined,
      disabled: !clickable && n.children.length === 0,
      onClick: clickable
        ? () => {
            ctxMenuOpen = false;
            void invoke<MenuResult>("invoke_ctx_item", { id: n.id }).then((r) => handleMenuResult(r, []));
          }
        : undefined,
    };
  };

  // 查看子菜单：8 种视图，圆点标记当前项（写回 ShellBag 由 setView 完成）
  const vi = (view: ViewMode, label: string, glyph: string): MenuItem => ({
    label, glyph, checked: tab.view === view, checkStyle: "dot", onClick: () => setView(view),
  });
  const viewSub: MenuItem[] = [
    vi("xl-icons", "超大图标", "&#xE71D;"),
    vi("l-icons", "大图标", "&#xE922;"),
    vi("m-icons", "中图标", "&#xF0E2;"),
    vi("s-icons", "小图标", "&#xE80A;"),
    vi("list", "列表", "&#xEA37;"),
    vi("details", "详细信息", "&#xE8FD;"),
    vi("tiles", "平铺", "&#xE8A9;"),
    vi("content", "内容", "&#xE8A4;"),
  ];

  // 排序方式子菜单：与命令栏"排序"菜单一致
  const sortItem = (key: SortKey, label: string): MenuItem => ({
    label, checked: tab.sortKey === key, checkStyle: "dot",
    onClick: () => { tab.sortKey = key; renderHeader(); renderList(); },
  });
  const sortSub: MenuItem[] = [
    sortItem("name", "名称"),
    sortItem("date", "修改日期"),
    sortItem("type", "类型"),
    sortItem("size", "大小"),
    { separator: true },
    { label: "递增", checked: tab.sortAsc, checkStyle: "dot", onClick: () => { tab.sortAsc = true; renderHeader(); renderList(); } },
    { label: "递减", checked: !tab.sortAsc, checkStyle: "dot", onClick: () => { tab.sortAsc = false; renderHeader(); renderList(); } },
  ];

  // 新建子菜单：ShellNew 模板（与命令栏"新建"菜单同源）；虚拟位置回退仅"文件夹"
  let fileIdx = 0;
  const newSub: MenuItem[] = newEntries.length
    ? newEntries.map((e) => {
        if (e.separator) return { separator: true } as MenuItem;
        const i = fileIdx++;
        return {
          label: e.label,
          iconImg: e.icon ?? undefined,
          glyph: e.icon ? undefined : (i === 0 ? "&#xE8B7;" : "&#xE7C3;"),
          onClick: () => {
            void invoke("invoke_new_item", { id: e.id }).then(() => {
              setTimeout(() => void refresh(), 500);
              setTimeout(() => void refresh(), 1500);
            });
          },
        } as MenuItem;
      })
    : [{ label: "文件夹", glyph: "&#xE8B7;", disabled: virt, onClick: () => void createNewFolder() }];

  // 扩展段：背景树中的现代扩展（在终端中打开/Code 等白名单），其余收进"显示更多选项"
  const ext: MenuItem[] = [];
  for (const n of tree) {
    if (n.separator) continue;
    if (!isModernExt(n.label)) continue;
    ext.push(nodeToItem(n));
  }

  // "显示更多选项"：完整经典背景菜单（头部与资源管理器一样补 查看/排序方式/刷新）
  const moreSub: MenuItem[] = [
    { label: "查看", submenu: viewSub },
    { label: "排序方式", submenu: sortSub },
    { label: "刷新", onClick: () => void refresh() },
    { separator: true },
    ...tree.map(nodeToItem),
  ];

  const items: MenuItem[] = [
    { label: "查看", glyph: "&#xE890;", submenu: viewSub },
    { label: "排序方式", glyph: "&#xE8CB;", submenu: sortSub },
    { label: "刷新", glyph: "&#xE72C;", onClick: () => void refresh() },
    { separator: true },
  ];
  // 有可撤消操作时才显示"撤消"（与 Win11 资源管理器一致）
  const top = undoStack[undoStack.length - 1];
  if (top) {
    items.push({ label: `撤消${top.label}`, glyph: "&#xE7A7;", accel: "Ctrl+Z", onClick: () => void doUndo() });
  }
  items.push({ label: "新建", glyph: "&#xE710;", submenu: newSub });
  items.push({ separator: true });
  // 文件夹自身属性：背景菜单打开前已清空选中，showProperties 会回落到当前文件夹；
  // 不能用背景菜单对象按 verb 调用（其"属性"项无法按 properties verb 解析，会静默失败）
  items.push({ label: "属性", glyph: "&#xE90F;", accel: "Alt+Enter", onClick: showProperties });
  if (ext.length) items.push({ separator: true }, ...ext);
  if (tree.length) {
    items.push({ separator: true }, { label: "显示更多选项", glyph: "&#xE712;", submenu: moreSub });
  }

  // 有可粘贴对象时才显示顶部快捷条（仅"粘贴"，靠左），否则整条隐藏
  showCtxMenuAt(x, y, items, [], canPaste && !virt ? buildBgQuickbar() : null);
  ctxUiOpen = true;
  ctxMenuOpen = tree.length > 0;
}

function handleMenuResult(r: MenuResult, sel: string[]) {
  const tab = activeTab();
  if (r.action === "navigate" && sel.length === 1) {
    void navigate(sel[0]);
  } else if (r.action === "rename") {
    startRename();
  } else if (r.action === "set-view") {
    setView(r.verb as ViewMode);
  } else if (r.action === "set-sort") {
    tab.sortKey = r.verb as Tab["sortKey"];
    renderHeader(); renderList();
  } else if (r.action === "set-sort-dir") {
    tab.sortAsc = r.verb === "asc";
    renderHeader(); renderList();
  } else if (r.action === "refresh") {
    void refresh();
  } else if (r.action === "invoked") {
    if (r.verb === "cut") { cutPaths = new Set(sel); renderList(); return; }
    if (r.verb === "copy") return;
    cutPaths = new Set();
    setTimeout(() => void refresh(), 500);
    setTimeout(() => void refresh(), 1500);
  }
}

async function runVerb(verb: string) {
  // 虚拟位置（此电脑等）不支持剪贴板/删除类操作（含快捷键路径）
  if (isVirtualLocation()) return;
  const tab = activeTab();
  const sel = [...tab.selection];
  if (verb === "paste") {
    await invoke("invoke_verb", { selection: [], background: tab.listing?.parse_path ?? null, verb });
    cutPaths = new Set();
    setTimeout(() => void refresh(), 500);
    setTimeout(() => void refresh(), 1500);
    return;
  }
  if (sel.length === 0) return;
  await invoke("invoke_verb", { selection: sel, background: null, verb });
  if (verb === "cut") { cutPaths = new Set(sel); renderList(); }
  else if (verb === "copy") { cutPaths = new Set(); renderList(); }
  else {
    cutPaths = new Set();
    setTimeout(() => void refresh(), 500);
    setTimeout(() => void refresh(), 1500);
  }
}

function startRename() {
  const tab = activeTab();
  const sel = [...tab.selection];
  if (sel.length !== 1) return;
  const path = sel[0];
  // 支持所有视图：找到项目元素及其名称元素（详细信息/图标/列表/平铺/内容）
  const row = document.querySelector<HTMLElement>(`#list-body [data-path="${CSS.escape(path)}"]`);
  const entry = tab.listing?.entries.find((e) => e.parse_path === path);
  if (!row || !entry) return;
  const nameSpan = row.querySelector<HTMLElement>(
    ".cell-name span, .grid-name, .list-name, .tile-name, .content-name",
  );
  if (!nameSpan) return;
  const input = document.createElement("input");
  input.className = "rename-input";
  input.value = entry.name;
  nameSpan.replaceWith(input);
  input.focus();
  // 选中不含扩展名的部分（与资源管理器一致）
  const dot = entry.name.lastIndexOf(".");
  input.setSelectionRange(0, entry.is_folder || dot <= 0 ? entry.name.length : dot);

  let done = false;
  const commit = async () => {
    if (done) return; done = true;
    const newName = input.value.trim();
    if (newName && newName !== entry.name) {
      try {
        await invoke("rename_item", { path, newName });
        // 记录可撤消：改回原名（仅限真实文件系统路径）
        const sep = path.lastIndexOf("\\");
        if (sep > 0) {
          const newPath = path.slice(0, sep + 1) + newName;
          const oldName = entry.full_name || entry.name;
          undoStack.push({
            label: "重命名",
            run: async () => { await invoke("rename_item", { path: newPath, newName: oldName }); },
          });
        }
      } catch (e) { console.error(e); }
    }
    void refresh();
  };
  input.onblur = () => void commit();
  input.onkeydown = (ev) => {
    ev.stopPropagation();
    if (ev.key === "Enter") void commit();
    if (ev.key === "Escape") { done = true; void refresh(); }
  };
  input.onclick = (ev) => ev.stopPropagation();
  input.ondblclick = (ev) => ev.stopPropagation();
}

async function createNewFolder() {
  const tab = activeTab();
  const parent = tab.listing?.parse_path;
  if (!parent) return;
  const existing = new Set(tab.listing!.entries.map((e) => e.name));
  let name = "新建文件夹";
  for (let i = 2; existing.has(name); i++) name = `新建文件夹 (${i})`;
  try {
    await invoke("create_folder", { parent, name });
    await refresh();
    const created = tab.listing?.entries.find((e) => e.name === name);
    if (created) {
      const createdPath = created.parse_path;
      // 记录可撤消：删除新建的文件夹（回收站）
      undoStack.push({
        label: "新建文件夹",
        run: async () => { await invoke("invoke_verb", { selection: [createdPath], background: null, verb: "delete" }); },
      });
      tab.selection = new Set([created.parse_path]);
      renderList();
      renderStatus();
      startRename();
    }
  } catch (e) { console.error(e); }
}

/* ===================== 标签页 ===================== */
function addTab(path: string) {
  tabs.push(newTab(path));
  activeTabIdx = tabs.length - 1;
  void navigate(path, { push: false });
}
function closeTab(i: number) {
  if (tabs.length === 1) { void appWindow.close(); return; }
  tabs.splice(i, 1);
  if (activeTabIdx >= tabs.length) activeTabIdx = tabs.length - 1;
  else if (i < activeTabIdx) activeTabIdx--;
  renderAll();
}

/* ===================== 侧栏加载 ===================== */
async function loadSidebar() {
  sidebar = await invoke<SidebarData>("get_sidebar");
  renderSidebar();
  const all = [...sidebar.quick_access, ...sidebar.drives];
  // 网络/Linux 节点的系统图标（显示器/企鹅）也一并提取
  if (sidebar.network) all.push(sidebar.network);
  if (sidebar.linux) all.push(sidebar.linux);
  await loadIcons(all, () => new Map());
  renderSidebar();
}

/* ===================== 事件绑定 ===================== */
function bindEvents() {
  $("win-min").onclick = () => void appWindow.minimize();
  $("win-max").onclick = () => void appWindow.toggleMaximize();
  $("win-close").onclick = () => void appWindow.close();
  // 同步最大化图标。注意：不要在最大化时 setResizable(false)——摘掉 WS_SIZEBOX 会让
  // 系统判定窗口不可贴靠，Snap Layout（悬停/Win+Z）全部失效；
  // tao 0.35 的 WM_NCHITTEST 已自带 !is_maximized 保护，最大化时不会出现顶边 resize 光标
  const syncMaximized = async () => {
    const m = await appWindow.isMaximized();
    $("win-max-glyph").innerHTML = m ? "&#xE923;" : "&#xE922;";
    // 上报最大化状态：最大化时后端在顶边盖一条本进程覆盖窗口，
    // 屏蔽 WebView2 自己实现的边缘 resize 带（它不经过宿主命中测试，
    // 运行时关闭设置又需导航才生效）；还原时隐藏，边缘拖拽调大小照常
    void invoke("set_window_maximized", { maximized: m });
  };
  void appWindow.onResized(() => void syncMaximized());
  void syncMaximized();

  // 自定义标题栏拖拽：最大化时真正拖动（超过阈值）才还原为窗口并跟随鼠标；
  // 单击不触发还原，双击切换最大化（Windows 原生行为）
  const dragEl = $("titlebar-drag");
  dragEl.addEventListener("mousedown", async (ev) => {
    if (ev.button !== 0 || ev.detail >= 2) return; // 双击交给 dblclick 处理
    ev.preventDefault();
    if (!(await appWindow.isMaximized())) {
      await appWindow.startDragging();
      return;
    }
    // 最大化：监听移动，超过 4px 才还原并开始拖拽
    const sx = ev.clientX;
    const sy = ev.clientY;
    let started = false;
    const cleanup = () => {
      document.removeEventListener("mousemove", onMove);
      document.removeEventListener("mouseup", onUp);
    };
    const onUp = () => cleanup();
    const onMove = async (e: MouseEvent) => {
      if (started) return;
      if (Math.abs(e.clientX - sx) < 4 && Math.abs(e.clientY - sy) < 4) return;
      started = true;
      cleanup();
      // 记录鼠标物理坐标与在标题栏中的比例位置
      const scale = await appWindow.scaleFactor();
      const pos = await appWindow.outerPosition();
      const cursorX = pos.x + e.clientX * scale;
      const cursorY = pos.y + e.clientY * scale;
      const ratioX = e.clientX / window.innerWidth;
      await appWindow.unmaximize();
      // 还原后按比例把窗口放到鼠标下方，再开始系统拖拽
      const size = await appWindow.outerSize();
      await appWindow.setPosition(new PhysicalPosition(
        Math.round(cursorX - size.width * ratioX),
        Math.round(cursorY - sy * scale),
      ));
      await appWindow.startDragging();
    };
    document.addEventListener("mousemove", onMove);
    document.addEventListener("mouseup", onUp);
  });
  dragEl.addEventListener("dblclick", () => void appWindow.toggleMaximize());

  $("nav-back").onclick = goBack;
  $("nav-fwd").onclick = goForward;
  $("nav-up").onclick = goUp;
  $("nav-refresh").onclick = () => void refresh();

  $("tab-add").onclick = () => addTab(THIS_PC);

  $("cmd-new").onclick = (ev) => void showNewMenu(ev.currentTarget as HTMLElement);
  $("cmd-cut").onclick = () => void runVerb("cut");
  $("cmd-copy").onclick = () => void runVerb("copy");
  $("cmd-paste").onclick = () => void runVerb("paste");
  $("cmd-rename").onclick = () => startRename();
  $("cmd-delete").onclick = () => void runVerb("delete");
  $("cmd-share").onclick = (ev) => {
    const r = (ev.currentTarget as HTMLElement).getBoundingClientRect();
    void showItemMenu(r.left, r.bottom + 4);
  };
  $("cmd-sort").onclick = (ev) => showSortMenu(ev.currentTarget as HTMLElement);
  $("cmd-view").onclick = (ev) => showViewMenu(ev.currentTarget as HTMLElement);
  $("cmd-more").onclick = (ev) => showMoreMenu(ev.currentTarget as HTMLElement);
  // 命令栏右侧"详细信息"：开关详细信息窗格（与资源管理器一致）
  $("cmd-details").onclick = () => {
    settings.detailsPane = !settings.detailsPane;
    if (settings.detailsPane) settings.previewPane = false;
    renderAll();
  };

  // 地址栏：点击空白处进入编辑模式（面包屑段/箭头/根目录图标除外）
  $("breadcrumb-box").onclick = (ev) => {
    if (!(ev.target as HTMLElement).closest(".crumb, .crumb-chev, .crumb-loc-icon")) startAddressEdit();
  };

  // 状态栏视图切换按钮
  const statusBtns = document.querySelectorAll<HTMLElement>(".statusbar .status-btn");
  statusBtns[0]?.addEventListener("click", () => setView("details"));
  statusBtns[1]?.addEventListener("click", () => setView("l-icons"));

  const search = $("search-input") as HTMLInputElement;
  search.oninput = () => {
    activeTab().filter = search.value;
    renderList();
    renderStatus();
  };
  search.onkeydown = (ev) => ev.stopPropagation();

  document.addEventListener("keydown", (ev) => {
    const tab = activeTab();
    if (ev.ctrlKey && ev.key.toLowerCase() === "a") {
      ev.preventDefault();
      tab.selection = new Set(sortedEntries(tab).map((e) => e.parse_path));
      renderList(); renderStatus();
    } else if (ev.ctrlKey && ev.shiftKey && ev.key.toLowerCase() === "c") { copyAddresses(); }
    else if (ev.ctrlKey && ev.key.toLowerCase() === "c") { void runVerb("copy"); }
    else if (ev.ctrlKey && ev.key.toLowerCase() === "z") { void doUndo(); }
    else if (ev.ctrlKey && ev.key.toLowerCase() === "x") { void runVerb("cut"); }
    else if (ev.ctrlKey && ev.key.toLowerCase() === "v") { void runVerb("paste"); }
    else if (ev.ctrlKey && ev.key.toLowerCase() === "t") { addTab(THIS_PC); }
    else if (ev.ctrlKey && ev.key.toLowerCase() === "w") { closeTab(activeTabIdx); }
    else if (ev.key === "Delete") { void runVerb("delete"); }
    else if (ev.key === "F2") { startRename(); }
    else if (ev.key === "F5") { void refresh(); }
    else if (ev.altKey && ev.key === "Enter") { showProperties(); }
    else if (ev.key === "Enter") {
      const sel = [...tab.selection];
      if (sel.length === 1) {
        const e = tab.listing?.entries.find((x) => x.parse_path === sel[0]);
        if (e) void openEntry(e);
      }
    }
    else if (ev.key === "Backspace" || (ev.altKey && ev.key === "ArrowLeft")) { goBack(); }
    else if (ev.altKey && ev.key === "ArrowRight") { goForward(); }
    else if (ev.altKey && ev.key === "ArrowUp") { goUp(); }
  });

  // 分栏拖拽：侧栏（左）与详细信息窗格（右）；宽度持久化，重启/刷新后保持上次拖拽的宽度
  const setupResizer = (handle: HTMLElement, target: HTMLElement, min: number, max: number, invert: boolean, storeKey: string) => {
    // 恢复上次宽度
    const saved = Number(localStorage.getItem(storeKey));
    if (saved >= min && saved <= max) target.style.width = `${saved}px`;
    handle.onmousedown = (ev) => {
      ev.preventDefault();
      const startX = ev.clientX;
      const startW = target.getBoundingClientRect().width;
      document.body.style.cursor = "ew-resize";
      const move = (e: MouseEvent) => {
        const dx = e.clientX - startX;
        const w = invert ? startW - dx : startW + dx;
        target.style.width = `${Math.min(max, Math.max(min, w))}px`;
      };
      const up = () => {
        document.removeEventListener("mousemove", move);
        document.body.style.cursor = "";
        localStorage.setItem(storeKey, String(Math.round(target.getBoundingClientRect().width)));
      };
      document.addEventListener("mousemove", move);
      document.addEventListener("mouseup", up, { once: true });
    };
  };
  setupResizer(
    document.querySelector<HTMLElement>(".sidebar-resizer")!,
    document.querySelector<HTMLElement>(".sidebar")!,
    140, 480, false, "sidebarWidth",
  );
  setupResizer($("pane-resizer"), $("side-pane"), 220, 620, true, "paneWidth");

  // 详细信息视图水平滚动时，表头跟随同步
  $("list-body").addEventListener("scroll", () => {
    $("list-header").scrollLeft = $("list-body").scrollLeft;
  });

  // 鼠标框选
  setupMarquee();

  // 原生拖拽（drop 目标命中测试与高亮）
  setupNativeDnD();

  // 压制 Chromium 对 <img> 等元素的自发 HTML5 拖拽（与原生 OLE 拖拽冲突）
  document.addEventListener("dragstart", (ev) => ev.preventDefault());

  // 目录变更自动刷新（防抖）
  let fsTimer: number | undefined;
  void listen("fs-changed", () => {
    clearTimeout(fsTimer);
    fsTimer = window.setTimeout(() => void refresh(), 350);
  });

  // Snap Layout / 顶边条：上报窗口控制按钮矩形（客户区物理像素），尺寸/DPI 变化时重报；
  // 覆盖窗口接管后 HTML 按钮收不到鼠标事件，hover 背景由后端事件驱动
  const reportMaxBtnRect = () => {
    const s = window.devicePixelRatio;
    const px = (el: HTMLElement) => {
      const r = el.getBoundingClientRect();
      return {
        x: Math.round(r.left * s),
        y: Math.round(r.top * s),
        w: Math.round(r.width * s),
        h: Math.round(r.height * s),
      };
    };
    const mx = px($("win-max"));
    void invoke("set_max_button_rect", mx);
    const mn = px($("win-min"));
    const cl = px($("win-close"));
    void invoke("set_caption_rects", { minX: mn.x, minW: mn.w, closeX: cl.x, closeW: cl.w });
  };
  window.addEventListener("resize", reportMaxBtnRect);
  reportMaxBtnRect();
  void listen<boolean>("snap-hover", (ev) => {
    $("win-max").classList.toggle("nc-hover", ev.payload);
  });
  // 顶边条悬停最小化/关闭按钮时的高亮（顶边条屏蔽了 HTML :hover）
  void listen<string>("nc-btn-hover", (ev) => {
    $("win-min").classList.toggle("nc-hover", ev.payload === "min");
    $("win-close").classList.toggle("nc-hover", ev.payload === "close");
  });

  // 禁用 WebView 默认右键菜单
  document.addEventListener("contextmenu", (ev) => ev.preventDefault());
}

/* ===================== 启动 ===================== */
async function init() {
  bindEvents();
  // "在新窗口中打开"新实例：启动参数带初始路径时直接定位
  const start = await invoke<string | null>("get_start_path").catch(() => null) ?? THIS_PC;
  tabs.push(newTab(start));
  activeTabIdx = 0;
  void invoke("init_drag_drop");
  void invoke("init_snap_layout");
  await Promise.all([navigate(start, { push: false }), loadSidebar()]);
}

void init();
