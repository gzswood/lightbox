<div align="center">

# LightBox 桌面收纳盒

**把散在桌面上的快捷方式收进一条长条 —— Windows 轻量桌面整理工具**

单文件 exe，绿色免安装，约 **1.0 MB**。所有收纳盒合成**一条长条**，整条嵌进任务栏。

[![版本](https://img.shields.io/badge/version-1.0.1-4C8BF5?style=flat-square)](https://gitee.com/gathon/lightbox/releases)
[![平台](https://img.shields.io/badge/platform-Windows%2010%20%2F%2011-0078D6?style=flat-square&logo=windows&logoColor=white)](#系统要求)
[![协议](https://img.shields.io/badge/license-AGPL--3.0-blue?style=flat-square)](./LICENSE)
[![Rust](https://img.shields.io/badge/Rust-1.77%2B-000000?style=flat-square&logo=rust&logoColor=white)](https://www.rust-lang.org/)

[下载最新版](https://gitee.com/gathon/lightbox/releases) · [功能](#功能) · [使用](#使用) · [构建](#构建) · [常见问题](#常见问题) · [更新记录](docs/version_list.md) · [GitHub 镜像](https://github.com/gzswood/lightbox)

</div>

***

> **v1.0.0 是彻底重写的一版**：从 Tauri + React 全量换成 **纯 Rust + Win32**，
> 不再捆绑 WebView2，不再有 Node 进程。exe 从 3.9 MB 降到 **约 1.0 MB**，
> 进程树专用内存从约 464 MB（15 个进程）降到 **约 13 MB（1 个进程）**。
>
> **下载与自动更新都以 Gitee 为准。** 代码同时镜像在
> [GitHub](https://github.com/gzswood/lightbox)，但安装包只发布在 Gitee ——
> exe 里的检查更新地址是编译期写死的（`native/update-url.txt`）。

***

> **完全免费**，没有广告、没有捆绑、没有常驻后台服务。程序不收集任何使用数据，
> 唯一一次联网是启动后静默查一次版本号（可在托盘菜单「检查更新…」看结果）。

> **绿色版**：不写系统目录，不装服务，只有可选的「开机自启」会加一条注册表启动项。
> 配置文件和收纳的快捷方式都在程序 exe 文件根目录，boxes 文件夹保存的是拖进去的快捷方式。

## 简介

LightBox 解决一个很小但很麻烦的问题：桌面图标越堆越多，查找程序得瞄好几眼。

它把桌面上的图标归类收进**收纳盒**，收纳盒不再是东一个西一个散在桌面上，而是
**合成一条横向长条**，整条**嵌在任务栏里**，像一排书签。点长条上的某一段，
弹出这一段的图标面板；再点一次收起。面板里单击直接启动程序。

程序是**单文件绿色版**：整个 lightbox.exe 约 **1.0 MB**，双击即用，不需要安装。
配置（`config.json`）和收纳的快捷方式（`boxes\` 目录）都放在 exe 旁边，
把整个文件夹拷到别的机器上就能接着用。

支持 **Windows 10 与 Windows 11**（含 Win11 任务栏），多屏 / 混合缩放均可用。

## 功能

### 一条长条（核心设计）

- **整条长条** —— 所有收纳盒排成一条横向长条，不再各自散落在桌面上。
  新建一个盒子，长条就多一个分段，宽度随之增加
- **整条嵌进任务栏** —— 长条是任务栏的**真子窗口**（Win11 走 owner 挂载，Win10 走
  `SetParent`），所以它始终待在任务栏里，不挡桌面，也不会被别的窗口盖住
- **任务栏内容变了位置自己跟着走** —— 长条是任务栏的一部分，任务栏里多开一个
  任务视图、或者启动别的程序，Windows 把任务栏按钮重新排布，长条的位置会**自动跟着更新**，
  不会像「贴图」那样留在原地错位
- **横向滚动，宽度自适应屏幕** —— 分段太多放不下时，长条可以横向滚动；
  可视宽度按当前屏幕的工作区算
- **点哪段弹哪盒** —— 点长条上的第 N 段，弹出第 N 个盒子的面板；
  面板永远不盖住自己这一段
- **长条整体置顶 / 降级浮窗** —— 默认常驻任务栏；任务栏不是底部横条时，
  长条降级为浮在桌面上的普通窗口，仍可拖动

### 收纳盒

- **拖进去就收纳** —— 从桌面把 `.lnk` / `.url` 快捷方式拖到长条上，
  **移动**进 `boxes\{盒子id}\`，桌面立刻清爽
- **点开即用** —— 面板里单击启动程序；右键单个图标可「打开文件所在位置」「属性」
  「放回桌面」「从盒子移除」
- **面板内拖动排序** —— 安卓桌面那种：拖起来的那一格跟手浮起，被经过的格子依次让位，
  松手后顺序记进配置
- **框选与多选** —— 面板空白处按住拖出矩形即可框选；`Ctrl` + 单击加选/取消选，
  `Ctrl` + 框选追加，`Ctrl` + `A` 全选。选中后右键可对整批操作
  （打开全部 / 放回桌面 / 从盒子移除），也可以整批拖出面板
- **拖出到桌面** —— 按住图标拖出面板，走系统原生 OLE 拖拽，可以落到桌面或任意资源管理器窗口；
  多选后拖其中任意一个，**整批**一起出去
- **新建 / 删除 / 解散** —— 长条右键「新建收纳盒」；空盒可直接删除；
  非空盒可以「解散收纳盒」，图标全部释放回桌面
- **自定义盒子图标** —— 右键「设置图标…」选一个 `.ico`，设过之后菜单多出「恢复默认图标」

### 弹出面板

- **自适应格子数** —— 面板按内容数量算行列，格子始终是正方形，不多不少
- **头部三件套** —— 标题、「⋯」更多菜单（按名称排序 / 解散收纳盒）、关闭
- **主题跟随** —— 面板背景色、透明度、毛玻璃、圆角都在设置里改，改完立刻生效
- **Esc 收起** —— 点空白处、点关闭按钮、按 `Esc` 都能收起

### 外观

- **背景色自定义** —— 「收纳盒」和「弹出窗口」两个色块各自独立，内置取色器：
  常用色 → 饱和/明度方块 → 色相条 → hex 输入 → **吸管**（可以在屏幕任意位置点取颜色）
- **字色自动跟随** —— 盒子名字和面板文字都按底板/背景色的 WCAG 相对亮度自动选深字还是白字，
  并推导出次要文字、分隔线、强调色。背景调成什么颜色都不会出现字和背景撞色
- **弹窗字体** —— 「外观」里可设弹出面板的字体与基准字号（默认微软雅黑 12 px，可调到 20），
  标题、图标名、数量角标、空盒提示、改名框一起跟随，其它文字按基准字号等比缩放
- **透明度 / 毛玻璃 / 圆角** 三个开关，改完立刻生效，没有「保存」按钮。
  毛玻璃是按屏幕原尺寸抓整块再降采样 + 高斯模糊 + 双线性放大，背景均匀，不会出现被放大的残影
- **跟随系统缩放** —— 100% / 125% / 150% 都按物理像素计算；双屏混合缩放下尺寸和位置也准。
  切换显示模式后自动按新屏的工作区与缩放换算坐标

### 程序行为

- **系统托盘菜单** —— 显示主界面 / 隐藏主界面 / 设置… / 开机自启 / 检查更新… /
  打开数据目录 / 退出（新建收纳盒、关于在长条右键和设置页里）
- **单实例** —— 重复双击不会叠出第二套长条
- **开机自启** —— 写一条 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`，开关在设置和托盘里
- **更新提醒不弹窗** —— 程序启动时静默去 Gitee 仓库读一次 `version.json`；
  有新版本只在**面板标题栏显示「有更新」小牌**、设置页「关于」给出状态与说明。
  点小牌或托盘「检查更新…」都跳到「关于」页，绝不主动开窗、不抢焦点
- **快捷方式正确启动** —— 先解析 `.lnk` 拿到「目标 + 参数 + 工作目录」再拉起程序，
  并滤掉 `ELECTRON_*` / `NODE_OPTIONS` 这类会被子进程继承、导致 Electron 应用秒退的环境变量
- **命令行排障** —— `lightbox.exe --version` / `--paths` / `--dump-config` / `--check-update`
  四个开关，不用开界面就能查状态

## 系统要求

| 项 | 要求 |
| --- | --- |
| 系统 | Windows 10 64 位 / Windows 11 64 位 |
| 运行时 | **不需要额外运行时**（不依赖 WebView2 / .NET / Node） |
| 权限 | 不需要管理员权限 |
| 磁盘 | 约 1 MB，加你自己的快捷方式 |

## 使用

1. 从 [Releases](https://gitee.com/gathon/lightbox/releases) 下载 `lightbox.exe`，放到一个**固定的文件夹**里
   （比如 `D:\LightBox`），双击运行。不要放在「下载」目录里，清理下载文件夹会把它一起删掉
2. 托盘会出现一个图标，**任务栏里出现一条长条**（默认带「系统工具」「办公工具」两个空盒）
3. 从桌面把快捷方式拖到长条上 —— 图标会消失，因为文件已经**移动**进盒子里了
4. 点长条上的某一段，弹出这一盒的面板；单击图标启动程序；再点一次（或者按 `Esc`）收起面板
5. **右键长条分段**：打开 / 设置… / 置顶显示 / 设置图标… / 恢复默认图标 /
   从长条移除（保留文件） / 删除收纳盒（连同文件） / 打开数据目录
6. **右键长条空白处**：新建收纳盒 / 设置… / 打开数据目录
7. **托盘图标右键**：显示主界面 / 隐藏主界面 / 设置… / 开机自启 / 检查更新… /
   打开数据目录 / 退出（「新建收纳盒」在长条右键里，「关于」在设置页里）

> 删除盒子前会检查是否为空。盒子里还有东西时菜单项是灰的 —— 想把图标全倒回桌面，
> 用面板右上「⋯」里的**解散收纳盒**。

## 数据与备份

程序读写的文件都在 **exe 同一个目录**（exe 放在没有写权限的位置时，自动退回 `%LOCALAPPDATA%\LightBox`，
设置页的「存储位置」显示的就是实际在用的那个目录）。**第一次启动**（目录里只有 `lightbox.exe`）时，
会自动建好 `config.json`、`boxes\`（里面是「系统工具」「办公工具」两个默认盒的目录）、
`lightbox.log`、`update.json`：

| 文件 | 内容 | 要备份吗 |
| --- | --- | :---: |
| `config.json` | 盒子列表、顺序、外观设置 | **要** |
| `boxes\{盒子id}\*.lnk` | 盒子里实际收纳的快捷方式文件 | **要** |
| `config.backup.json` | 每次启动时的配置快照，误操作后可以拿回来 | 可选 |
| `config.before-v5.json` | 升级到长条模型之前的旧配置备份 | 可选 |
| `lightbox.log` | 运行日志，排障用 | 不用 |

**换机器或重装**：把 `config.json` 和 `boxes` 目录一起拷过去即可，盒子布局和里面的图标都在。
只拷 `config.json` 会得到一堆空盒子。

**升级版本**：程序不会自动安装，需**手动下载 `lightbox.exe` 覆盖旧文件**。
覆盖前请先退出托盘里的 LightBox，并**备份 `config.json` + `boxes\` 目录** ——
从 0.9.x 升到 1.0.0 时配置文件会在首次启动时迁移且不可回退，详见下方「常见问题」。

## 构建

要求：Windows 10/11 x64、**Rust 1.77+（MSVC 工具链）**。不需要 Node，不需要 WebView2。

```bash
git clone https://gitee.com/gathon/lightbox.git          # 国内访问更快
# 镜像：git clone https://github.com/gzswood/lightbox.git
cd lightbox

cd native
cargo build --release
```

产物：`native/target/release/lightbox.exe`（单文件，约 1.0 MB，直接可用）。

开发模式（带调试输出）：

```bash
cd native
cargo run
```

跑测试：

```bash
cd native
cargo test
```

发布流程（打 tag + 传安装包到 Gitee 发行版）见 [`tools/release-gitee.sh`](tools/release-gitee.sh)，
它会自动构建并打包 native 产物。

## 常见问题

<details>
<summary><b>点长条没反应？</b></summary>

先确认长条真的在任务栏里 —— 任务栏内容多的时候长条会被挤走，
可以点托盘菜单「显示 / 隐藏长条」切换一次，或者用滚轮横向滚动找它。

长条是任务栏的子窗口，Windows 任务栏本身被占用（比如任务栏正在弹出搜索框）时会暂时不响应，
属于正常现象。

</details>

<details>
<summary><b>长条的位置不对 / 跑到屏幕中间了？</b></summary>

长条跟着任务栏走。如果任务栏被改成了竖着放或浮动，长条会降级成桌面上的普通窗口，
右键长条可以拖动。

改回底部横条任务栏后，长条会自动回到任务栏里。

</details>

<details>
<summary><b>升级新版本会不会丢我的盒子配置？</b></summary>

**盒子和图标不会丢**：`config.json` 和 `boxes\` 目录跟 exe 是分开的，覆盖 exe 不动它们
（实测 5 个盒子 / 74 个 `.lnk` 完整保留）。

但 **v1.0.0 是配置模型变更，且不可回退**：把「N 个盒子各自摆放」换成「一条长条」，
首次启动会把 `config.json` 从 v3 迁到 v5 —— 丢弃 `group` / `group_pos`、
各盒 `x/y` 统一、`in_bar` 归 false，并写入 `bar`。**升上去以后不能再退回 0.9.x 使用**。

所以覆盖前请把整个 exe 目录（`config.json` + `boxes\`）复制一份。
程序自己会留 `config.before-v5.json` 和 `config.backup.json`，但前者存的是 v4 阶段、
后者每次启动都会被覆盖，**都不能代替完整备份**。想回到 0.9.x，请用你自己的备份还原。

</details>

<details>
<summary><b>杀毒软件报警 / 提示未知发布者？</b></summary>

exe 没有买代码签名证书（个人项目，证书一年几千块），所以 Windows 会显示「未知发布者」，
部分杀软对不认识的单文件 exe 也会先拦一下。加信任即可。

想彻底放心可以自己编译：源码全在仓库里，`cargo build --release` 出来的就是同一个东西。

</details>

<details>
<summary><b>点了图标没反应？</b></summary>

如果弹了「快捷方式指向的程序已不存在」，说明那个软件被卸载或换了位置 —— 把快捷方式删掉重新建一个。

如果什么提示都没有，检查一下：快捷方式本身在资源管理器里双击能不能打开。
LightBox 是解析 `.lnk` 拿到真实目标再启动的，比直接双击更严格。

</details>

<details>
<summary><b>支持多个显示器吗？两个屏缩放比例不一样呢？</b></summary>

支持。所有坐标都是**物理像素**，按所在显示器的实际缩放算，125% + 150% 混插也不会跑到错误的屏上。
切换显示模式（笔记本 ↔ 外接、双屏 ↔ 单屏、更换主屏）后会自动换算到新屏。

</details>

<details>
<summary><b>会不会自己弹窗催我更新？</b></summary>

不会。检查更新全程静默，有新版本只在**设置窗口的版本号**那里显示一行红色文字，
你自己决定点不点。托盘菜单里有「检查更新…」可以手动查。

</details>

## 技术栈

| 分类 | 选型 |
| --- | --- |
| 语言 | **Rust**（全栈，无前端） |
| 应用框架 | **无** —— 直接 Win32 窗口 + 一个消息循环 |
| 依赖 | `windows` 0.62（Win32 FFI）、`serde` / `serde_json`、`image`、`base64`、`uuid` |
| 窗口 | 分层窗口（`WS_EX_LAYERED` + `UpdateLayeredWindow`），自绘全部 UI |
| 任务栏 | Win11 走 owner 挂载，Win10 走 `SetParent`，长条成为任务栏真子窗口 |
| DPI | `DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2`，坐标一律物理像素 |
| 网络 | 手写 WinHTTP FFI（`http.rs`），**零第三方网络依赖** |
| 持久化 | `serde_json` → `config.json` |
| 图标提取 | `SHGetFileInfoW` + `HICON` → BGRA 位图 |
| 构建 | `cargo build --release`（`opt-level=s` + LTO + strip） |

**为什么不用现成框架？** 旧版用 Tauri v2 + React，运行时要拉起一组 WebView2 进程，
整棵进程树的专用内存合计约 464 MB（15 个进程），Win11 上还会偶发卡顿、点击无响应。
纯 Win32 重写后是**单进程、单消息循环**，没有第二个进程要调度，
专用内存约 13 MB，exe 也从 3.9 MB 缩到约 1.0 MB。

## 目录结构

```
native/               原生实现（Rust）
  src/main.rs         入口：单实例锁、DPI 初始化、启动长条
  src/app.rs          主窗口与消息循环、托盘菜单
  src/bar.rs          长条（任务栏胶囊）：分段布局、滚动、绘制
  src/taskbar.rs      任务栏探测、挂载/卸载、托盘区边界
  src/panel.rs        弹出面板：格子布局、框选多选、拖动排序
  src/dnd.rs          OLE 拖放：拖入收纳 / 拖出到桌面
  src/config.rs       config.json 读写、盒子目录、版本迁移
  src/settings.rs     设置窗口（名称 / 颜色 / 透明度 / 开关）
  src/picker.rs       自绘取色器 + 吸管
  src/theme.rs        主题：由背景色推导字色、分隔线、强调色
  src/render.rs       画布：位图绘制、圆角、文本
  src/glass.rs        毛玻璃（DWM Acrylic）
  src/icons.rs        图标提取与缓存
  src/shortcut.rs     .lnk 解析与进程启动
  src/desktop.rs      桌面目录、桌面图标刷新
  src/menu.rs         右键菜单
  src/tray.rs         托盘图标
  src/updater.rs      版本检查（静默、不弹窗）
  src/http.rs         WinHTTP 封装
  src/autostart.rs    开机自启（注册表 Run 键）
  src/log.rs          日志落盘
  src/collate.rs      中文名称排序
  src/geom.rs         坐标与缩放换算
  src/iconpick.rs     .ico 文件选择对话框
  assets/app.rc       嵌入 exe 的图标资源
update-site/          更新清单 version.json（程序直接读 Gitee raw，不用自建站点）
                      ⚠️ 目录名沿用旧称，路径已编译进 exe，不能改名或移动
tools/                Gitee 一键发版脚本
docs/                 更新记录与截图
```

## 开源协议

Copyright (C) 2026 Gathon

本项目以 **[GNU AGPL-3.0](https://www.gnu.org/licenses/agpl-3.0.html)** 协议开源，协议全文见 [LICENSE](LICENSE)。

你可以自由使用、修改、分发，自己用或者公司内部用没有任何额外义务。AGPL-3.0 是强 copyleft 协议：
**基于本项目修改后的版本如果对外分发，必须以同样的协议公开源代码**；把它部署成网络服务供他人使用时，
也要提供对应源码。这条约束的目的是防止有人拿它做闭源的二次打包、或者包装成收费软件。

## 更多

- 仓库（主）：<https://gitee.com/gathon/lightbox>
- 代码镜像：<https://github.com/gzswood/lightbox>（只托管代码，不发布安装包）
- 版本下载：<https://gitee.com/gathon/lightbox/releases>
- 问题反馈：<https://gitee.com/gathon/lightbox/issues>
- 更新记录：[docs/version_list.md](docs/version_list.md)

<div align="center">
<img src="docs/images/xcoder.jpg" width="256" alt="微信公众号：爱学习的xcoder" />

作者：Gathon

微信：gzswood

微信公众号：爱学习的xcoder
</div>
