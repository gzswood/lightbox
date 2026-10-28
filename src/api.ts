import { invoke } from "@tauri-apps/api/core";
import type { Theme } from "./theme";

export type Item = {
  name: string;
  file: string;
  path: string;
  icon: string | null;
};

export type BoxView = {
  id: string;
  name: string;
  count: number;
  topmost: boolean;
  /** 已嵌入任务栏（横向胶囊形态，只能沿任务栏横向移动） */
  in_bar: boolean;
  /** 舞台设计尺寸（逻辑像素）：普通盒子 80×80；
   *  任务栏盒子 = 任务栏厚度换算的高、加宽后的宽（右侧放两个汉字） */
  w: number;
  h: number;
  /** 嵌入任务栏时隐藏名称（设置里的开关）：true 时胶囊只画图标 */
  bar_hide_label: boolean;
  /** 自定义图标（png data URL）；为空时用内置图标 */
  icon: string | null;
  /** 属于某个「胶囊团」：拖动前要先问后端整团偏移，拖着一起走 */
  in_group: boolean;
};

export type ImportReport = {
  added: Item[];
  skipped: string[];
};

export type MenuData = {
  id: string;
  name: string;
  count: number;
  topmost: boolean;
  /** 已嵌入任务栏（菜单里显示「移出任务栏」） */
  in_bar: boolean;
  /** 是否设过自定义图标（决定菜单里出不出「恢复默认图标」） */
  has_icon: boolean;
  /** 已经在某个胶囊团里（菜单显示「解散胶囊团」） */
  grouped: boolean;
  /** 旁边有吸附在一起的邻居、还没组过（菜单显示「组合为胶囊团」） */
  can_group: boolean;
};

export type AppSettings = {
  autostart: boolean;
  exe: string;
  data_dir: string;
  version: string;
};

/** 取色器启动信息：全屏快照 + 虚拟屏幕矩形 + 取色窗口 DPI 系数 */
export type PickStart = {
  shot: string;
  x: number;
  y: number;
  w: number;
  h: number;
  scale: number;
};

/** 一次取样：光标周围的原图 + 光标处颜色 */
export type PickSample = {
  shot: string;
  color: string;
};

/** 面板当前展示哪个盒子 + 本次显示的序号（换盒子时用来「先换内容再显示」） */
export type PanelTarget = {
  id: string;
  seq: number;
};

/** 更新检查状态（来自服务器上的 version.json） */
export type UpdateState = {
  /** 本机版本 */
  current: string;
  /** 服务器上的最新版本；空 = 还没成功拿到过 */
  latest: string;
  date: string;
  notes: string[];
  download: string;
  page: string;
  has_update: boolean;
  /** 上次检查时间（毫秒时间戳） */
  checked_at: number;
  error: string;
  busy: boolean;
  /** false = 还没配置更新地址 */
  configured: boolean;
};

export const api = {
  view: (id: string) => invoke<BoxView>("get_box_view", { id }),
  /** 盒子页面画完首帧后回报：切置顶时后端要等这一拍才显示新窗口，避免闪白 */
  boxReady: (id: string) => invoke<void>("box_ready", { id }),
  items: (id: string) => invoke<Item[]>("list_items", { id }),
  importPaths: (id: string, paths: string[]) =>
    invoke<ImportReport>("import_paths", { id, paths }),
  open: (path: string) => invoke<void>("open_item", { path }),
  /** 在资源管理器里定位这个图标并选中（.lnk 跳它指向的程序所在目录） */
  reveal: (path: string) => invoke<void>("reveal_item", { path }),
  /** 弹出系统的「属性」对话框，和资源管理器右键「属性」完全一致 */
  properties: (path: string) => invoke<void>("item_properties", { path }),
  /** 批量「放回桌面」（多选后一次操作），返回成功个数 */
  restoreMany: (id: string, files: string[]) =>
    invoke<number>("restore_items", { id, files }),
  /** 批量「从盒子移除」（直接删除），返回成功个数 */
  removeMany: (id: string, files: string[]) =>
    invoke<number>("remove_items", { id, files }),
  /** 「全部图标移回桌面」：整个盒子清空回桌面，返回成功个数 */
  restoreAll: (id: string) => invoke<number>("restore_all_items", { id }),
  /** 保存面板里拖动后的图标顺序（文件名列表） */
  reorder: (id: string, files: string[]) => invoke<void>("reorder_items", { id, files }),
  /** 开始把快捷方式拖出盒子（原生 OLE 拖拽，支持多选一批拖出），返回 moved/copied/cancel */
  dragOut: (id: string, files: string[]) => invoke<string>("drag_item_out", { id, files }),
  createBox: (name: string) => invoke<BoxView>("create_box", { name }),
  renameBox: (id: string, name: string) => invoke<void>("rename_box", { id, name }),
  deleteBox: (id: string) => invoke<void>("delete_box", { id }),
  /** 兜底落盘：把盒子当前真实位置写进配置（位置由后端用 Win32 取，
   *  前端不要再传坐标 —— tao 的 onMoved 给的是父客户区坐标，差一整块屏）。 */
  moveBox: (id: string) => invoke<void>("move_box", { id }),
  /** 盒子所在显示器的缩放系数（100% = 1.0）。页面用它把 80×80 的舞台缩到正确物理尺寸。 */
  boxScale: (id: string) => invoke<number>("box_scale", { id }),
  /** 盒子附近壁纸的亮度（0 = 纯黑，1 = 纯白）。名字据此在白字 / 深色字之间切换，
   *  和桌面图标一样「字色跟着壁纸走」。null = 读不到（窗口刚销毁 / 点全在屏幕外）。 */
  labelLuma: (id: string) => invoke<number | null>("label_luma", { id }),
  /** 拖动松手后吸附（贴屏幕边 / 贴别的盒子）并落盘坐标。
   *  null = 鼠标还按着（稍后再问）；true = 吸了（要重截背景）；false = 不用动。 */
  snapBox: (id: string) => invoke<boolean | null>("snap_box", { id }),
  /** 任务栏盒子的落位：横坐标夹进任务栏、纵坐标钉在任务栏上沿 */
  snapBar: (id: string) => invoke<boolean | null>("snap_bar", { id }),
  /** 拖动途中的轮询（每 60ms 一次）：悬停够时间就在**不松手**的情况下切换形态。
   *  null = 继续问；"idle" = 停（拖动结束/不归它管）；
   *  "embed" / "unembed" = 已切换，窗口正在重建，别再问了。 */
  barHover: (id: string) =>
    invoke<"idle" | "embed" | "unembed" | null>("bar_hover", { id }),
  setTopmost: (id: string, onTop: boolean) =>
    invoke<void>("set_box_topmost", { id, onTop }),
  /** 嵌入 / 移出任务栏 */
  setBoxInBar: (id: string, on: boolean) =>
    invoke<void>("set_box_in_bar", { id, on }),
  openPanel: (id: string) => invoke<void>("open_panel", { id }),
  /** 打开「设置」独立窗口（盖在面板中央） */
  openSettings: (id: string) => invoke<void>("open_settings", { id }),
  /** 托盘「设置…」：全局设置窗口（公共项，不含单盒名称） */
  openGlobalSettings: () => invoke<void>("open_global_settings"),
  /** 托盘「自动排序」：所有盒子回屏幕底部居中横排 */
  autoSort: () => invoke<void>("auto_sort"),
  hideSettings: () => invoke<void>("hide_settings"),
  /** 按内容高度调整设置窗口（逻辑像素），后端负责重设尺寸 + 相对面板重新居中 */
  fitSettings: (height: number) => invoke<void>("fit_settings", { height }),
  /** 设置窗口重载后恢复「我在给哪个盒子做设置」 */
  settingsTarget: () => invoke<string | null>("get_settings_target"),
  /** 面板重载后恢复「我在显示哪个盒子」 */
  panelTarget: () => invoke<PanelTarget | null>("get_panel_target"),
  /** 让桌面上的盒子直接进入改名状态（右键菜单「重命名」） */
  beginBoxRename: (id: string) => invoke<void>("begin_box_rename", { id }),
  /** 新盒子的内容已经画完首帧：后端收到后才真正显示面板，避免闪出上一个盒子的图标 */
  panelReady: (id: string, seq: number) => invoke<void>("panel_ready", { id, seq }),
  /** 拖出到桌面后把前台抢回面板，这样再点桌面时失焦事件才会来（面板才收得起） */
  refocusPanel: () => invoke<void>("refocus_panel"),
  /** 读外观主题（背景色 / 毛玻璃 / 透明度） */
  theme: () => invoke<Theme>("get_theme"),
  /** 保存外观主题：写配置 + 广播给所有窗口 */
  setTheme: (theme: Theme) => invoke<void>("set_theme", { theme }),
  /** 毛玻璃底图：面板背后那块屏幕的模糊快照（关掉毛玻璃时为 null） */
  backdrop: () => invoke<string | null>("panel_backdrop"),
  /** 前台窗口是不是本程序自己的窗口（判断失焦该不该收起面板） */
  foregroundIsSelf: () => invoke<boolean>("foreground_is_self"),
  /** 左键是否按着：桌面图标往面板拖时，按下那下面板就会失焦，先别收 */
  mouseLeftDown: () => invoke<boolean>("mouse_left_down"),
  /** 光标是否在面板窗口里：拖拽松手时落在面板内就保持打开 */
  cursorInPanel: () => invoke<boolean>("cursor_in_panel"),
  /** 光标是否停在某个收纳盒（含任务栏胶囊）上 —— 用来区分「用户要点盒子」和「用户离开」 */
  cursorOnBox: () => invoke<boolean>("cursor_on_box"),
  /** 左 Ctrl 的物理按键状态：WebView2 偶尔不给鼠标事件带修饰键，Ctrl+点击拿它兜底 */
  ctrlDown: () => invoke<boolean>("ctrl_down"),
  // ---------- 胶囊团 ----------
  /** 把旁边吸附成一片的盒子组合成一个整体；返回成员个数 */
  groupBoxes: (id: string) => invoke<number>("group_boxes", { id }),
  /** 解散整个胶囊团（成员恢复成可以单独拖动）；返回解散的个数 */
  ungroupBox: (id: string) => invoke<number>("ungroup_box", { id }),
  /** 拖动开始前算好整团偏移（必须早于 startDragging）；返回是否真的在团里 */
  groupDragBegin: (id: string) => invoke<boolean>("group_drag_begin", { id }),
  /** 拖动途中每次窗口移动：把其它成员跟到「锚点 + 偏移」 */
  groupFollow: (id: string) => invoke<boolean>("group_follow", { id }),
  /** 这次拖动没走吸附时，收个尾（对齐 + 落盘） */
  groupEnd: (id: string) => invoke<void>("group_end", { id }),
  closePanel: () => invoke<void>("close_panel"),
  /** 在光标处弹出盒子右键菜单（独立窗口） */
  showBoxMenu: (id: string) => invoke<void>("show_box_menu", { id }),
  hideMenu: () => invoke<void>("hide_menu"),
  /** 菜单渲染后把真实内容高度回报给后端，修正首次显示的公式估值 */
  fitMenu: (h: number) => invoke<void>("fit_menu", { height: h }),
  menuData: () => invoke<MenuData | null>("get_menu_data"),
  /** 选 .ico 设置收纳盒图标；返回 data URL，取消返回 null */
  pickBoxIcon: (id: string) => invoke<string | null>("pick_box_icon", { id }),
  /** 恢复内置的收纳盒图形 */
  clearBoxIcon: (id: string) => invoke<void>("clear_box_icon", { id }),
  settings: () => invoke<AppSettings>("get_settings"),
  setAutostart: (on: boolean) => invoke<void>("set_autostart", { on }),
  openDataDir: () => invoke<void>("open_data_dir"),
  quit: () => invoke<void>("quit_app"),
  // ---------- 版本更新 ----------
  /** 当前更新状态（页面挂载时拉一次；之后靠 update:data 事件推送） */
  updateState: () => invoke<UpdateState>("update_state"),
  /** 手动检查（绕过节流，立即联网） */
  checkUpdate: () => invoke<UpdateState>("check_update"),
  /** 打开（或聚焦）更新窗口 */
  openUpdateWindow: () => invoke<void>("open_update_window"),
  /** 用系统浏览器打开更新网页（软件站点首页，下载入口在网页上） */
  openUpdateSite: () => invoke<void>("open_update_site"),
  hideUpdate: () => invoke<void>("hide_update"),
  // ---------- 屏幕取色器（吸管） ----------
  /** 进入取色：后端收起设置窗口、截一张全屏快照返回；窗口等 pickReady 才显示 */
  beginPick: () => invoke<PickStart>("begin_pick"),
  /** 快照已经铺好：让后端显示取色窗口（早一帧会闪） */
  pickReady: () => invoke<void>("pick_ready"),
  /** 取色窗口页面挂载时拿「当前这次取色会话」的快照（事件在页面加载前会丢） */
  pickStartNow: () => invoke<PickStart | null>("pick_start_now"),
  /** 鼠标移动：取样光标周围 15×15 原图 + 中心点颜色（传窗口内 CSS 坐标） */
  pickSample: (x: number, y: number) => invoke<PickSample>("pick_sample", { x, y }),
  /** 左键点击：取当前屏幕上该点的真实颜色，通过 pick:done 事件发给设置窗口 */
  pickCommit: (x: number, y: number) => invoke<void>("pick_commit", { x, y }),
  /** 右键 / Esc：取消取色 */
  pickCancel: () => invoke<void>("pick_cancel"),
};
