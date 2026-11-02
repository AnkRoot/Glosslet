set positional-arguments

# 列出统一开发入口；默认不构建、不联网、不读取选区。
default:
    @just --list

# 格式化 Rust 与 justfile。
fmt:
    python3 tests/acceptance.py fmt
    just --fmt

# 宏装配模块不会被 cargo fmt 自动发现；显式检查全部 Rust 文件，不手写文件清单。
check-fmt:
    python3 tests/acceptance.py fmt --check
    just --fmt --check

# 从装配声明发现功能，拒绝 core 反向依赖与跨功能深路径导入。
check-architecture:
    python3 tests/acceptance.py architecture

# 严格静态检查。
lint:
    cargo clippy --all-targets --locked -- -D warnings

# 自动测试：只用本地响应服务与自有管道，不读取桌面选区。
test:
    cargo test --all-targets --locked

# Debug 构建。
build:
    cargo build --locked

# Release 构建。
build-release:
    cargo build --release --locked

# 清理构建产物，不触碰配置或桌面集成。
clean:
    cargo clean

# 与 CI 一致：局部服务、结构检查和自有 Xvfb，不读真实选区或剪贴板，不请求翻译。
verify: check-fmt check-architecture lint test test-release check-integration test-runtime

# 发布归档、AUR 校验和与 .SRCINFO 测试：使用自有合成文件，不发布、不安装。
test-release:
    python3 -m unittest discover -s tests -p '*_test.py'

# 真正运行程序的隔离控制／面板验收；参数可明确选择自有 Xvfb 可执行文件。
[linux]
test-runtime xvfb="Xvfb": build-release
    python3 tests/acceptance.py runtime --xvfb "$1"

# 从唯一结构定义导出平铺配置；check 只比较，write 只写本工作区，不部署用户配置。
integration mode="check":
    #!/usr/bin/env python3
    import json
    from pathlib import Path
    import re
    from string import Template
    import sys
    import tomllib

    mode = sys.argv[1]
    if mode not in {"check", "write"}:
        raise SystemExit("模式只能为 check 或 write")
    root = Path("integration")
    data = json.loads((root / "desktops.json").read_text(encoding="utf-8"))
    assert set(data) == {"shortcut", "desktops", "plugins"}, "集成 schema 字段无效"
    shortcut = data["shortcut"]
    assert set(shortcut) == {"modifiers", "key"}
    modifiers, key = shortcut["modifiers"], shortcut["key"]
    assert modifiers and len(modifiers) == len(set(modifiers))
    assert set(modifiers) <= {"Super", "Control", "Alt", "Shift"}
    assert set(modifiers) & {"Super", "Control", "Alt"}, "必须是明确组合键"
    assert isinstance(key, str) and len(key) == 1 and key.isascii() and key.isalpha()
    package = tomllib.loads(Path("Cargo.toml").read_text(encoding="utf-8"))["package"]
    binary = package["name"]
    identity_pattern = json.dumps("^" + re.escape(binary) + "$")

    def binding(name):
        if name == "niri":
            names = {"Super": "Mod", "Control": "Ctrl", "Alt": "Alt", "Shift": "Shift"}
            return "+".join([*(names[mod] for mod in modifiers), key.upper()])
        if name == "hyprland":
            names = {"Super": "SUPER", "Control": "CTRL", "Alt": "ALT", "Shift": "SHIFT"}
            return " + ".join([*(names[mod] for mod in modifiers), key.upper()])
        if name in {"sway", "i3"}:
            names = {"Super": "Mod4", "Control": "Control", "Alt": "Mod1", "Shift": "Shift"}
            return "+".join([*(names[mod] for mod in modifiers), key.lower()])
        if name == "gnome":
            return "".join("<" + mod + ">" for mod in modifiers) + key.lower()
        if name == "kde":
            names = {"Super": "Meta", "Control": "Ctrl", "Alt": "Alt", "Shift": "Shift"}
            return "+".join([*(names[mod] for mod in modifiers), key.upper()])
        if name == "xdg-autostart":
            return ""
        raise ValueError("缺少此桌面的原生快捷键格式")

    files, names = set(), set()
    for desktop in data["desktops"]:
        required = {"name", "file", "comment", "note", "template"}
        assert required <= set(desktop) <= required | {"source"}
        name, filename = desktop["name"], desktop["file"]
        assert name not in names and filename not in files, "重复桌面或产物"
        assert Path(filename).name == filename and not filename.startswith("."), "产物必须全量平铺"
        assert desktop["comment"] in {"#", "//", "--"}
        names.add(name)
        files.add(filename)
        comment = desktop["comment"]
        banner = comment + " 由 integration/desktops.json 生成；只编辑结构定义，不手改产物。\n"
        banner += comment + " " + desktop["note"] + "\n"
        banner += comment + " 程序须能由 PATH 找到；不会自动写入用户桌面配置。\n\n"
        content = banner + Template(desktop["template"]).substitute(binary=binary, product=binary.capitalize(), lookup=binary + " lookup", binding=binding(name), identity_pattern=identity_pattern)
        if "source" in desktop:
            content += Path(desktop["source"]).read_text(encoding="utf-8")
        path = root / filename
        if mode == "write":
            path.write_text(content, encoding="utf-8")
        elif not path.exists() or path.read_text(encoding="utf-8") != content:
            raise SystemExit("集成产物缺失或偏离结构定义：" + filename)
    def substitute(value):
        if isinstance(value, str):
            return Template(value).substitute(binary=binary, product=binary.capitalize(), version=package["version"], repository=package["repository"])
        if isinstance(value, dict):
            return {key: substitute(item) for key, item in value.items()}
        if isinstance(value, list):
            return [substitute(item) for item in value]
        return value

    for plugin in data["plugins"]:
        assert set(plugin) == {"name", "file", "archive", "metadata", "files"}
        filename = plugin["file"]
        assert Path(filename).name == filename and filename not in files
        files.add(filename)
        content = json.dumps(substitute(plugin["metadata"]), ensure_ascii=False, indent=2) + "\n"
        path = root / filename
        if mode == "write":
            path.write_text(content, encoding="utf-8")
        elif not path.exists() or path.read_text(encoding="utf-8") != content:
            raise SystemExit("插件元数据偏离结构定义：" + filename)
    unexpected = {path.name for path in root.iterdir()} - files - {"desktops.json"}
    assert not unexpected, "integration/ 存在非平铺或废弃路径：" + str(unexpected)
    print("平铺集成结构验证通过：" + ", ".join(sorted(files)))

# 无真实桌面与用户配置写入的结构、Lua 接线、Desktop Entry、GVariant 槽位检查。
check-integration: (integration "check")
    #!/usr/bin/env python3
    import ast
    import configparser
    from pathlib import Path
    import re
    import subprocess
    import tomllib

    subprocess.run(["desktop-file-validate", "integration/autostart.desktop"], check=True)
    subprocess.run(["node", "--input-type=module", "--check"], input=Path("src/lookup/gnome/extension.js").read_text(encoding="utf-8"), encoding="utf-8", check=True)
    subprocess.run(["node", "--check", "integration/kde.js"], check=True)
    config = configparser.ConfigParser(interpolation=None)
    config.read("integration/gnome.dconf", encoding="utf-8")
    assert len(config.sections()) == 1
    for value in config[config.sections()[0]].values():
        assert isinstance(ast.literal_eval(value), str)
    binary = tomllib.loads(Path("Cargo.toml").read_text(encoding="utf-8"))["package"]["name"]
    subprocess.run(["lua", "-", "^" + re.escape(binary) + "$"], input=r'''
    local startup, bound, registered, layer
    local hl = {
        layer_rule = function(rule)
            assert(layer == nil and rule.match.namespace == arg[1])
            for prop in pairs(rule.match) do assert(prop == "namespace") end
            assert(rule.no_anim == true and rule.blur == false and rule.blur_popups == false)
            assert(rule.dim_around == false and rule.above_lock == 0)
            -- Layer 没有 float/pin/no_focus 等普通窗口属性；不能伪造它们。
            for prop in pairs(rule) do
                assert(({ match = true, no_anim = true, blur = true, blur_popups = true, dim_around = true, above_lock = true })[prop])
            end
            layer = rule
        end,
        exec_cmd = function(command) startup = command end,
        dsp = { exec_cmd = function(command) return { command = command } end },
        on = function(event, callback)
            assert(event == "hyprland.start" and registered == nil)
            registered = callback
        end,
        bind = function(keys, action, flags)
            assert(bound == nil and type(keys) == "string")
            assert(flags.repeating == false)
            bound = action.command
        end,
    }
    assert(loadfile("integration/hyprland.lua", "t", { hl = hl }))()
    assert(layer and registered and startup == nil)
    registered()
    assert(type(startup) == "string" and bound == startup .. " lookup")
    ''', encoding="utf-8", check=True)
    print("集成语法与接线检查通过；不代表各 compositor 真机验收。")

# 只读检查所选原生后端的协议／接口，不读选区、不映射面板，可在已有实例下执行。
check-desktop:
    cargo run --release --locked -- --check-desktop

# 启动开发实例，或传入应用参数；真实查词会向选定服务发送选区。
run *args:
    cargo run --locked -- "$@"

# Release 运行与控制；真实查词会向选定服务发送选区。
run-release *args:
    cargo run --release --locked -- "$@"

# 去除 DISPLAY 后启动，确保只使用原生 Wayland。
[linux]
run-wayland *args:
    env -u DISPLAY cargo run --locked -- "$@"

# 查看应用参数；控制消息需要已有运行实例。
help:
    cargo run --locked -- --help

# 显示公开测试文案；需要已有实例，不读取选区、不发翻译请求。
probe-panel:
    cargo run --locked -- --probe-panel

# 只翻译公开词 hello；会联网，不属于 verify。
check-service service="mirror":
    cargo run --locked -- --check-service "$1"

# 打包桌面插件，只写 target/package；不安装到用户目录、不启用插件。
package-desktop: (integration "check")
    python3 release/package.py desktop

# 明确目标平台，打包原生程序；不安装到用户目录。
package platform: build-release (integration "check")
    python3 release/package.py portable "$1"

# 需要两种 Linux 的真实归档与 Arch makepkg；只生成 AUR 提交文件，不上传。
package-aur:
    python3 release/package.py aur

# 安装到 Cargo bin 目录；不会写入桌面配置。
install:
    cargo install --path . --locked

# Debian/Ubuntu 构建依赖；仅此显式入口需要管理员权限。
[linux]
install-deps-debian:
    sudo apt-get update
    sudo apt-get install -y pkg-config libxkbcommon-dev libxkbcommon-x11-0 fonts-noto-cjk lua5.4 desktop-file-utils nodejs xvfb libx11-6 libxtst6

# 原生 Niri 面板烟测：公开文案，不读选区、不联网，不属于 verify；拒绝已有实例。
[linux]
smoke-wayland: build-release
    #!/usr/bin/env python3
    import json
    import os
    from pathlib import Path
    import re
    import socket
    import subprocess
    import tempfile
    import time
    import tomllib

    app_id = tomllib.loads(Path("Cargo.toml").read_text(encoding="utf-8"))["package"]["name"]
    env = os.environ.copy()
    env.pop("DISPLAY", None)
    env["WAYLAND_DEBUG"] = "client"
    binary = str((Path("target/release") / app_id).resolve())
    runtime = Path(env["XDG_RUNTIME_DIR"])
    if list(runtime.glob(app_id + "-*.sock")):
        raise SystemExit("请先结束已有 Glosslet 实例；烟测不会控制其他实例或清理其 socket。")

    def query(request):
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
            connection.settimeout(2)
            connection.connect(env["NIRI_SOCKET"])
            connection.sendall((json.dumps(request) + "\n").encode())
            reply = json.loads(connection.makefile("rb").readline(1024 * 1024))
            return reply["Ok"][request]

    def focused_window():
        window = query("FocusedWindow")
        return None if window is None else window["id"]

    def panels():
        return [layer for layer in query("Layers") if layer["namespace"] == app_id]

    from runpy import run_path
    measure = run_path("tests/acceptance.py")["metrics"]
    samples = {}
    original_focus = focused_window()
    env.pop("WAYLAND_SOCKET", None)
    with tempfile.TemporaryDirectory(prefix=app_id + "-niri-test-") as temporary, tempfile.TemporaryFile(mode="w+b") as log:
        # Niri IPC 必须属于真实会话运行目录；只隔离偏好，不伪造目录或绕过身份检查。
        scratch = Path(temporary)
        env["XDG_CONFIG_HOME"] = str(scratch / "config")
        process = subprocess.Popen([binary], env=env, stdout=log, stderr=log)
        started = False

        def wait_for(condition):
            deadline = time.monotonic() + 3
            while time.monotonic() < deadline:
                if process.poll() is not None:
                    raise RuntimeError(os.pread(log.fileno(), 65536, 0).decode(errors="replace"))
                if condition():
                    return
                time.sleep(0.03)
            raise TimeoutError("Wayland 面板烟测超时")

        def ready():
            return "Glosslet：已启动" in os.pread(log.fileno(), 65536, 0).decode(errors="replace")

        def mapped():
            layers = panels()
            return len(layers) == 1 and layers[0]["keyboard_interactivity"] == "OnDemand"

        def control(*args):
            subprocess.run([binary, *args], env=env, check=True, timeout=2)

        try:
            wait_for(ready)
            started = True
            samples["startup_idle"] = measure(process.pid)
            assert not panels(), "启动时不应映射面板"
            begin = time.monotonic()
            control("--probe-panel")
            wait_for(mapped)
            samples["map_elapsed_ms"] = round((time.monotonic() - begin) * 1000, 2)
            samples["panel_visible"] = measure(process.pid)
            assert panels()[0]["layer"] == "Overlay"
            assert focused_window() == original_focus, "初始面板抢占了来源焦点"
            for action in ["pin", "pin", "unpin", "unpin"]:
                control(action)
            control("close")
            wait_for(lambda: not panels())
            samples["after_close"] = measure(process.pid)
            control("quit")
            assert process.wait(timeout=3) == 0
            trace = os.pread(log.fileno(), 65536, 0).decode(errors="replace")
            assert ".get_pointer(" in trace, "初始座位未绑定 pointer"
            assert ".get_data_device(" in trace, "初始座位未绑定 data-control"
            assert trace.count(".get_layer_surface(") >= 2, "采样面不能原地变为结果面板"
            assert ".set_destination(256, 100)" in trace, "面板逻辑尺寸不正确"
            assert trace.count(".frame(") == 1, "只允许首次映射的一次帧确认"
            assert not re.search(r"<- wl_keyboard@\d+\.enter\b", trace), "初始映射不应接收键盘焦点"
            assert ".receive(" not in trace, "面板诊断不应请求选区数据"
            report = Path("target/acceptance/niri.json")
            report.parent.mkdir(parents=True, exist_ok=True)
            report.write_text(json.dumps(samples, ensure_ascii=False, indent=2) + "\n")
            print("Wayland 面板烟测通过：无 DISPLAY，公开文案，保持焦点，关闭移除图层；未读取选区、未请求翻译。")
            print(json.dumps(samples, ensure_ascii=False, indent=2))
        except Exception:
            print(os.pread(log.fileno(), 65536, 0).decode(errors="replace"))
            print("当前 Glosslet 图层：", panels())
            raise
        finally:
            if process.poll() is None:
                if started:
                    try:
                        control("quit")
                    except (subprocess.SubprocessError, OSError):
                        process.terminate()
                else:
                    process.terminate()
                try:
                    process.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()

# 验证 i3 集成片段语法；不代表真实 X11 桌面验收。
[linux]
check-i3:
    i3 -C -c integration/i3.conf

# 验证 Sway 原生配置片段，不启动 compositor 或修改实际配置。
[linux]
check-sway:
    sway --validate --config integration/sway.conf

# 验证 Niri 集成示例，不修改正在使用的配置。
[linux]
check-niri:
    niri validate --config integration/niri.kdl
