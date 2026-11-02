# Glosslet

选中文字，按一次组合键，在阅读位置附近看译文，然后继续阅读。

Glosslet 是一个低打扰的桌面划词翻译工具：256×100 的轻量置顶面板、词典释义／句子翻译、复制、滚动、固定和明暗主题。只在明确触发时查词；固定后也不自动翻译新选区。不扫描全局按键，不监听修饰键双击，不保存翻译历史。

**目前是实验版。原生实现、交叉编译、CI 打包与真实桌面验收是不同层次，不能相互替代。** 新产品使用独立命令、配置目录、插件标识和仓库；不提供旧命令别名或配置迁移。

## 无需安装也能运行

“便携”指解压后直接运行，**不是把配置写在程序旁边**。配置仍放在系统标准目录；源码目录和二进制所在目录可以是任意位置。

正式 Release 尚未发布。发布后，原生归档将由 [AnkRoot/Glosslet](https://github.com/AnkRoot/Glosslet) 的标签构建产生，包含程序、许可证、说明和构建信息；Linux 还包含桌面集成片段及插件包。当前可以在工作区直接运行：

```sh
cargo build --release --locked
./target/release/glosslet
./target/release/glosslet status
./target/release/glosslet quit
```

不需要 `cargo install`、管理员权限或常驻安装服务。Linux 运行时需要系统的 xkbcommon 与可用字体；X11 后端另需 X11、xcb 和 xkbcommon-x11 运行库。桌面接入还需要对应协议、IPC 或插件，详见下文。Windows/macOS 原生归档尚未完成签名；macOS 还需要用户授予辅助功能权限。

如希望从 PATH 启动，可自行安装：

```sh
cargo install --path . --locked
```

**桌面进程的 PATH 不一定等于终端 PATH。** 自启动和快捷键应使用能被桌面找到的二进制，必要时写绝对路径。只有安装到 PATH 才能直接使用下文的 `glosslet`；未安装时用实际文件路径替换命令。

### Arch／AUR

计划使用 **`glosslet-bin`**，不是旧产品的同名包；当前 AUR 公开查询未占用，不等于已经发布或保留该名称。归档直接运行和 AUR 安装都保留系统标准配置目录。

Release 流程会用两种 Linux 的**真实发布归档**计算 SHA-256，生成 `PKGBUILD`，再由 Arch `makepkg --printsrcinfo` 派生 `.SRCINFO`。不使用 `SKIP`，不手写第二份元数据，不自动上传 AUR。包只安装程序、许可证、本文及 `/usr/share/glosslet/integration/` 示例，不写用户配置或自动启用自启动。

提交 AUR 还需要 AUR 账号、已登记的 SSH 公钥、有效公开下载地址、包名检查及本机 `makepkg` 验收。AUR 认证与 GitHub 认证相互独立。实际发布归档可用后，在 Arch 上：

```sh
# 把 Release 的两个 Linux 归档放入 target/package/ 后：
just package-aur
# 输出 target/package/aur/PKGBUILD、.SRCINFO，以及 AUR 提交归档。

# 在独立目录中验证生成的 PKGBUILD；会下载公开发布归档并安装构建依赖：
(cd target/package/aur && makepkg --cleanbuild --syncdeps)
```

审核通过后，再将生成文件提交到 `ssh://aur@aur.archlinux.org/glosslet-bin.git`。本程序不自行检查更新；更新由用户的包管理操作决定。

## 当前验证范围

| 环境 | 本次重构后的证据 | 仍待验收 |
|---|---|---|
| Linux · Niri 26.04 | 原生 Wayland 公开面板烟测：无 DISPLAY，采样面／结果面分离，首映射保持来源焦点，固定后不自动查词，关闭移除图层 | 实体组合键、重构后真实选区／翻译／粘贴闭环、应用矩阵和多输出 |
| Linux · X11 | 自有 Xvfb：真实程序启动、单实例、CLI、公开面板、主题按钮、幂等固定、损坏拒绝、重启及 12 次面板资源周期 | 真实 i3／X11 桌面、选区归属与粘贴、激活／拖动／关闭体验 |
| Linux · Hyprland、Sway | 原生 IPC、协议和过期事件的本地测试；不是实机支持声明 | 实际桌面完整验收 |
| Linux · GNOME Shell 51、KDE/KWin | 原生集成及插件打包、语法和接线检查 | 安装／启用／重启／卸载、实际选区与交互 |
| Windows | UI Automation、原生窗口与组合键；原生 CI 的严格 Clippy、单元测试与 Release 构建 | 真机权限、选区、窗口生命周期及输入交互 |
| macOS · Intel／Apple Silicon | Accessibility、AppKit 窗口与组合键；两种架构原生 CI 的严格 Clippy、单元测试与 Release 构建 | 两种架构真机权限、选区及交互 |

重构前的受控公开 `hello` 取词／粘贴闭环不能自动当作新版本完整验收。[首次通过的原生 CI](https://github.com/AnkRoot/Glosslet/actions/runs/41455779449) 已完成 Linux x86_64／aarch64、Windows x86_64、macOS Intel／Apple Silicon 的格式、严格 Clippy、适用单元测试与 Release 构建；最低 Rust 1.89 检查和两种 Linux 的自有 Xvfb 验收也已通过。发行标签的真实归档校验与 AUR 生成流程尚未执行，不声称五种公开发布归档已经产生。

Wayland PRIMARY 不提供选择所有者窗口身份，焦点和焦点 epoch 只能拒绝过期工作，**不能证明选区归属**。Niri／Sway 目前没有采用原生指针坐标接口，保留一次性的透明定位采样：它可能短暂截获点击或滚轮，不承诺无损输入。不会在失败后切换桌面、调用 XWayland／X11、模拟 Ctrl+C、读取普通剪贴板、OCR 或扫描全局输入。

## 桌面接入与使用

Glosslet 不会自动修改桌面配置。只合并必要片段，不覆盖整个文件、不重复添加自启动或快捷键。默认组合键为 **Super + Shift + G**，macOS 的 Super 对应 Command。

### Niri、Hyprland、Sway、i3

- Niri：[integration/niri.kdl](integration/niri.kdl)。分别合并启动、图层规则和绑定，不重复创建 `binds` 块。
- Hyprland：[integration/hyprland.lua](integration/hyprland.lua)，面向当前 0.57 Lua API。
- Sway：[integration/sway.conf](integration/sway.conf)。
- i3：[integration/i3.conf](integration/i3.conf)，只用于明确的原生 X11 会话。

组合键执行 `glosslet lookup`。Wayland 使用 layer-shell 覆盖层，X11 使用不参与平铺的覆盖窗口，不添加无效的普通窗口 floating／sticky 规则。Wayland 接入要求 layer-shell v4+、viewporter 与 ext-data-control-v1；缺少协议或桌面身份不唯一时明确失败。

Niri 的虚拟键盘实现直接把事件发给焦点窗口，不经过 compositor 绑定，因此虚拟按键、配置校验或 CLI 触发**都不能验收实体组合键**。

### Windows、macOS

应用只注册明确的组合快捷键。启动时可指定其他组合键：

```sh
glosslet --shortcut Control+Alt+G
```

只接受 Super／Control／Alt／Shift 与一个英文字母构成的组合键；占用或注册失败报错，不扫描键盘。Windows 用 UI Automation TextPattern，不模拟复制。macOS 用 Accessibility，需在“隐私与安全性 → 辅助功能”中授权；不请求录屏或输入监控权限。受限应用、不提供选区能力或权限不足时明确失败。

### GNOME、KDE

必须显式安装并启用原生插件：

```sh
just package-desktop
```

- GNOME：`target/package/glosslet@ankroot.shell-extension.zip`，面向 Shell 51。使用 `gnome-extensions install` 安装，启用 `glosslet@ankroot`；首次安装可能需要重新登录 Wayland 会话。另手工合并 [integration/gnome.dconf](integration/gnome.dconf)，把槽位追加到已有 `custom-keybindings` 列表，不覆盖列表。
- KDE：`target/package/glosslet.kwinscript`，通过 KWin 脚本页面安装和启用。脚本自行注册组合键，不重复创建 desktop 动作。

[integration/autostart.desktop](integration/autostart.desktop) 只提供常驻启动；用户可按需安装并将 Exec 改为绝对路径，不与桌面专用启动重复启用。程序不自动启用插件，也不绕过缺少的能力。

### 常用操作

1. 启动应用，启动不显示面板。
2. 在来源应用选中文字并保持其活动。
3. 按组合键查看译文。
4. 单击词典词条或双击句子复制；未固定时复制后关闭。

面板顶部可固定、切换主题、关闭；内容可滚动。**固定只是驻留，下一次查词仍需快捷键。** 拖动、Esc、外部点击和失焦行为仍须分别验收各桌面。

```sh
glosslet status       # 只返回阶段、面板与固定状态，不返回选区或译文
glosslet settings     # 运行中的偏好
glosslet pin          # 幂等固定，不触发查词
glosslet unpin
glosslet close
glosslet quit

glosslet set direction zh-en  # 自动中英，默认
glosslet set direction zh     # 中文目标
glosslet set direction en     # 英文目标
glosslet set direction ja     # 日文目标
glosslet set theme light
glosslet set theme dark
```

隐藏面板、取词中或当前绘制尚未确认时，不允许改变固定状态；关闭和重启不保留固定。自动中英仅判断汉字，不是通用语言识别；纯汉字日文请指定目标语言。已有请求和失败重试沿用触发时的方向及服务，不因修改偏好自动重新联网。目前没有托盘或图形设置窗口。

## 翻译服务与隐私

默认服务为 `https://v2g.borber.top`，可显式选择 Google 直连：

```sh
glosslet set service mirror
glosslet set service google
```

真实查词将选区发送给**所选服务**。失败不自动把原文发到另一个服务；失败面板可由用户重试。服务运营方、代理或网关可能记录请求，请勿翻译密码、密钥和敏感文档。镜像没有持续可用率或免费 SLA 保证，Google 直连依赖网络可达性；免费不等于准确性、延迟或长期可用性承诺。

成功、关闭或取消后按状态边界释放原文；失败重试仅保留当前请求需要的原文。复制只写译文，不读取既有剪贴板；退出后不保证译文仍可粘贴。过长选区报错，不静默截断。

## 会留下哪些文件

### 偏好

```sh
glosslet --config-path  # 无需已有实例；只显示路径，不创建文件
```

| 系统 | 设置文件 |
|---|---|
| Linux | `$XDG_CONFIG_HOME/glosslet/settings.json`，未设置时为 `~/.config/glosslet/settings.json` |
| Windows | `%APPDATA%\Glosslet\settings.json` |
| macOS | `~/Library/Application Support/Glosslet/settings.json` |

首次无文件时只用默认值；首次成功修改偏好或主题按钮操作才创建文件。严格 JSON，最大 4096 字节，三个字段必填、未知字段拒绝。临时文件同步后原子替换；Unix 目录为 `0700`、文件为 `0600`，验证所有者并拒绝符号链接。格式损坏或写入失败明确报错，运行值不变，不静默恢复默认、不覆盖损坏文件、不迁移旧格式。

建议通过命令修改。手工编辑应先退出应用：

```json
{
  "service": "mirror",
  "direction": "chinese_english",
  "theme": "dark"
}
```

### 运行时、安装与开发文件

- **Linux 运行时**：`$XDG_RUNTIME_DIR/glosslet-<会话散列>.sock` 与 `.lock`。同用户认证，请求／回复有界；正常退出删除 socket。空锁文件可保留到会话目录被清理，在线删除锁文件可能破坏单实例保证。异常退出残留 socket 只由同用户持锁的新实例回收。
- **macOS 运行时**：`~/Library/Caches/Glosslet/runtime/`，仅会话信道／锁，不是译文缓存。
- **Windows 运行时**：`%LOCALAPPDATA%\Glosslet\runtime\` 的锁文件，以及带私有 DACL、用户／会话认证的命名管道。
- **直接运行**：只有解压到所选目录的程序及附带文件，没有自动安装副本。
- **Cargo 安装**：程序位于 Cargo 的 bin 目录。**AUR 安装**：`/usr/bin/glosslet`、`/usr/share/doc/glosslet/`、许可证和集成示例，由包管理器追踪。
- **自启动／插件**：只有用户显式安装才写对应桌面目录；程序不自行部署。卸载时需要用户删除自己合并的启动和快捷键，并通过桌面工具卸载插件。
- **开发产物**：仓库 `target/` 包含编译、归档和 `target/acceptance/*.json` 公开测试报告；Python 可能产生 `__pycache__/`。这些不是程序运行缓存。

程序没有翻译历史、磁盘译文缓存或自建日志文件；输出仅到调用者的 stdout/stderr，调用者自行重定向的日志由调用者管理。字体／字形缓存仅在内存中，关闭面板释放本模块字体资源。系统字体缓存、其他应用数据与全局 Cargo 缓存不属于 Glosslet，不能随卸载一并删除。

卸载应先 `glosslet quit`，再使用对应安装方式卸载程序。保留或删除偏好由用户决定；不要在运行中删除锁文件，不扫描删除其他应用目录。本机旧构建与临时工具按约定等远程发布可用后再清理。

## 为什么会有多个线程和十几 MB 内存

原先测到的第三个常驻线程来自 Pango／Fontconfig 字体路径；RSS 约 33MB、PSS 约 15MB。此次删除 Linux 的重复 Cairo／Pango 绘制及依赖，各原生面板共用软件绘制，字体按需加载，关闭时释放字体／字形与面板缓冲区，而不是只隐藏窗口。

本机一次 Niri 公开文案测试（不是全平台指标）：

| 阶段 | RSS | PSS | 线程／fd |
|---|---:|---:|---:|
| 初始空闲 | 10.7MiB | 7.7MiB | 2／17 |
| 公开面板显示 | 14.9MiB | 9.6MiB | 2／18 |
| 固定／取消并关闭后 | 12.6MiB | 9.6MiB | 2／17 |

该次公开映射约 35ms，不是选区捕获延迟或 p95。自有 Xvfb 经 12 次面板周期后约 14.8MiB RSS，仍为 2 线程／17 fd；随后两秒 CPU tick 未增加。重测入口见下节。

RSS 包含共享库和映射页，PSS 按共享比例分摊，不能把 RSS 全算成私有内存。网络 DNS 可能临时启用阻塞线程，其他系统／桌面也可能有不同线程数；“两个线程”不是跨平台承诺。长期空闲、连续真实查词、取消、多语言字形与混合 DPI 仍须压力验收，不以冷启动数字代表运行后的占用。

## 结构与自动测试

运行路径是：**明确意图 → 当前选区 → 当前加载帧确认 → 所选翻译服务 → 结果面板 → 用户操作**。取词、内容、绘制和用户意图身份分离；取消和迟到事件不能操作新请求。原文在阶段边界释放，不把 UI 生命周期变成全局业务状态。

```text
src/main.rs              功能入口与装配单一声明
src/core.rs              共享数据、端口和装配值，不导入功能实现
src/control/             CLI 路由、认证信道、单实例和送达确认
src/settings/            严格文件契约与原子保存
src/translation/         显式服务、响应解析、网络与文本预算
src/panel/               展示模型、统一布局／绘制及可见资源
src/lookup/              查词状态与共享控制；原生桌面适配藏在内部
integration/desktops.json 桌面结构唯一来源，派生平铺片段／插件元数据
release/package.py       原生归档、插件及 AUR 文件，不部署用户配置
```

功能只通过 `core` 契约交互；查词显式依赖控制、偏好、翻译和展示端口。不同桌面保留真正不同的捕获、事件和 surface 生命周期，不为形式统一做平台回退。CLI 参数和帮助从同一份路由派生；无需手写第二份功能注册清单。宏装配不会被普通 `cargo fmt` 自动发现，因此统一入口显式发现全部 Rust 文件。

### 无真实桌面的自动验收

需要 Rust 1.89+、Python 3.11+ 和 just。Debian／Ubuntu 开发依赖：

```sh
just install-deps-debian  # 只有此显式入口需要管理员权限
just verify
```

Arch 开发环境可自行安装 `rust just python pkgconf libxkbcommon libxkbcommon-x11 noto-fonts-cjk lua desktop-file-utils nodejs xorg-server-xvfb libxtst`。验证不安装程序、不写实际用户设置、不读取真实选区／剪贴板、不请求在线翻译：

- 全部源码格式、严格 Clippy、功能入口／依赖方向、桌面生成物与 JavaScript／Lua／Desktop Entry 接线。
- Rust 状态机、原文释放、快速重触发、重试归属、过期帧／输入、私有文件、协议完整性／限长、认证及取消测试。
- HTTP 仅连接自有回环服务，验证编码、响应、限流、重定向拒绝、超长／慢响应及错误中不泄露原文。
- 自有 Xvfb 运行真实 Release：单实例、CLI、公开面板主题按钮、固定／关闭、损坏不覆盖、失败不改运行值、重启、资源周期及不完整控制帧。
- 合成程序头验证归档的**实际架构**而非只看文件名；插件资产及无用户数据夹带；Arch 上进一步用真实 makepkg 验证 AUR 校验和及派生 `.SRCINFO`。非 Arch 不冒充这项已完成。

可单独执行：

```sh
just test
just check-architecture
just test-release
just test-runtime                  # 自有 Xvfb，无真实桌面
just test-runtime /绝对路径/Xvfb    # 显式选择测试工具
just package-desktop
```

### 真实桌面测试手册

```sh
glosslet --check-desktop  # 只检查原生协议／接口，不读选区、不映射面板
glosslet --probe-panel    # 已有实例；仅公开文案，不联网
just smoke-wayland       # 当前 Niri 公开面板烟测，拒绝已有 Glosslet 实例

glosslet --check-service mirror  # 仅公开 hello；会联网，不属于 verify
```

真实验收必须在专用窗口中使用公开文本，不能读取测试前的剪贴板或既有敏感选区。记录版本、系统／桌面、来源应用、语言、输出几何／缩放、操作和结果；失败不能只重试到成功而丢弃记录。

| 项目 | 操作与通过条件 |
|---|---|
| 启动／实体快捷键 | 启动保持隐藏；选公开 `hello`，用实体组合键只触发一次，确认指令链路与所选服务。虚拟键盘不替代 compositor 绑定验收。 |
| 来源矩阵 | 浏览器正文、终端、编辑器、GTK/Qt、PDF 及不提供选区的应用；无选区／权限受限明确失败，不模拟复制。 |
| 文本／结果 | 英中、中英、日文显式方向、换行、Unicode、词典／句子／无结果、超过 64KiB；不串词、不静默截断。 |
| 复制／粘贴 | 词条单击、句子双击；只粘贴该次新复制的公开译文，不读取测试前剪贴板；未固定复制后关闭。 |
| 固定／主题／滚动 | 幂等固定和取消，不自动查新选区；主题保存，损坏时仍保持运行值；长结果可完整滚动。 |
| 手势／过期事件 | 拖动不变成按钮点击；按下后重绘、换词、关闭或调整缩放，旧释放事件不能操作新内容。 |
| 焦点／关闭 | 首映射不抢来源焦点；面板显式激活、Esc、外部点击、失焦、来源关闭分别记录，不混淆固定规则。 |
| 请求竞争 | 连续触发 A/B、加载未提交时关闭、失败重试时改偏好、请求中来源 ABA 变化；只接纳当前身份，原文按边界释放。 |
| 多输出 | 负坐标、混合 DPI、边缘／过小输出、旋转、热插拔；面板仍为 256×100 逻辑尺寸，不猜测输出或裁切。 |
| 定位采样风险 | Niri／Sway 采样期间连续公开点击／滚轮；记录可能丢失的输入和持续时间，不宣称无损，不把采样事件转为面板操作。 |
| 生命周期 | 锁屏、休眠、桌面 IPC／插件断开、重启与退出；失效明确终止，不跨桌面回退，任务／fd／线程／RSS/PSS 不持续累积。 |
| 插件／权限 | GNOME/KWin 安装、启用、重新登录、禁用／卸载；Windows/macOS 授权／拒绝、提升权限应用；只请求必要能力。 |

未能在真实环境执行的项目保留在 [TODO.md](TODO.md)，不得仅凭 CI 转为支持声明。

## GitHub 与发布

公开源码仓库为 **[AnkRoot/Glosslet](https://github.com/AnkRoot/Glosslet)**，以重构后的产品建立全新历史，不包含旧项目提交。默认分支推送和 Pull Request 触发验证，发行标签触发独立打包与发布；仓库创建和源码上传不等于 Release、AUR 或真实桌面验收完成。

Arch 上如缺少 `gh`：

```sh
sudo pacman -S github-cli
gh auth login
gh auth status
```

不要在聊天中粘贴 Token。维护者推送前应核对登录身份和仓库远程；发行必须使用本仓库的标签，不向其他项目的 origin 推送。

`.github/workflows/build.yml` 唯一定义五种原生构建矩阵，供验证与发布工作流复用。推送与 Cargo 版本一致的 `v<版本>` 标签后，先验证、构建、检查归档，再发布校验和及实验版资产；AUR 文件仅生成，不自动提交 AUR。签名、持续服务质量和真实桌面支持矩阵仍是发行前工作。

---

许可证：[GPL-3.0-only](LICENSE)。产品起点来自 [Borber/Tran](https://github.com/Borber/Tran)；全新命名不取消原项目的来源说明或许可证义务。
