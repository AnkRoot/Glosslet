#!/usr/bin/env python3
"""仓库验收入口。runtime 只接入自己创建的 Xvfb，绝不接入用户显示服务。

不执行 lookup，不请求翻译；公开面板、偏好文件、控制 framing 与生命周期是真实程序。
"""

import argparse
import ctypes
import ctypes.util
import json
import os
from pathlib import Path
import re
import select
import socket
import stat
import subprocess
import tempfile
import time
import tomllib

ROOT = Path(__file__).resolve().parents[1]
PACKAGE = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]
NAME = PACKAGE["name"]
DEFAULT = {"service": "mirror", "direction": "chinese_english", "theme": "dark"}


def fmt(check):
    paths = sorted((ROOT / "src").rglob("*.rs"))
    args = ["rustfmt", "--edition", PACKAGE["edition"]]
    if check:
        args.append("--check")
    subprocess.run([*args, *map(str, paths)], check=True, cwd=ROOT)


def architecture():
    main = (ROOT / "src/main.rs").read_text()
    declaration = re.search(r"features!\s*\{([^}]+)\}", main).group(1)
    features = re.findall(r"(\w+)\s*=>\s*(\w+)", declaration)
    names = {name for name, _ in features}
    assert len(names) == len(features), "重复功能声明"
    for name in names:
        entry = ROOT / "src" / name / "mod.rs"
        assert entry.is_file() and "fn register(" in entry.read_text(), f"缺少单一入口：{name}"
    for path in (ROOT / "src").rglob("*.rs"):
        owner = path.relative_to(ROOT / "src").parts[0]
        for target in re.findall(r"crate::(\w+)::", path.read_text()):
            assert target not in names or target == owner, f"跨功能实现依赖：{path} → {target}"
    core = (ROOT / "src/core.rs").read_text()
    assert not any(re.search(rf"\b{name}::", core) for name in names), "core 不能反向导入功能"
    print("功能入口、依赖方向与无跨功能深路径导入：通过")


def wait(predicate, message, timeout=8):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.03)
    raise TimeoutError(message)


def stop(process):
    if process is not None and process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=4)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=4)


class PrivateDisplay:
    """显示编号由自有 Xvfb 的 displayfd 返回，不读取或信任当前 DISPLAY。"""

    def __init__(self, scratch, executable):
        self.server = None
        self.connection = None
        self.log = (scratch / "xvfb.log").open("w")
        reader, writer = os.pipe()
        try:
            self.server = subprocess.Popen(
                [executable, "-displayfd", str(writer), "-screen", "0", "1280x720x24", "-nolisten", "tcp"],
                stdout=self.log, stderr=self.log, pass_fds=(writer,),
            )
            os.close(writer)
            writer = None
            if not select.select([reader], [], [], 8)[0]:
                raise TimeoutError("自有 Xvfb 未就绪")
            number = os.read(reader, 64).decode().strip()
            if not number.isdecimal() or self.server.poll() is not None:
                raise RuntimeError("Xvfb 未返回独立显示编号")
            self.name = ":" + number
            self.x = ctypes.CDLL(ctypes.util.find_library("X11"))
            self.xtest = ctypes.CDLL(ctypes.util.find_library("Xtst"))
            pointer, window, integer = ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int
            prototypes = {
                "XOpenDisplay": ([ctypes.c_char_p], pointer),
                "XDefaultRootWindow": ([pointer], window),
                "XCreateSimpleWindow": ([pointer, window, integer, integer, ctypes.c_uint, ctypes.c_uint, ctypes.c_uint, window, window], window),
                "XStoreName": ([pointer, window, ctypes.c_char_p], integer),
                "XMapRaised": ([pointer, window], integer),
                "XSetInputFocus": ([pointer, window, integer, window], integer),
                "XGetInputFocus": ([pointer, ctypes.POINTER(window), ctypes.POINTER(integer)], integer),
                "XWarpPointer": ([pointer, window, window, integer, integer, ctypes.c_uint, ctypes.c_uint, integer, integer], integer),
                "XSync": ([pointer, integer], integer),
                "XCloseDisplay": ([pointer], integer),
            }
            for name, (args, result) in prototypes.items():
                function = getattr(self.x, name)
                function.argtypes, function.restype = args, result
            self.xtest.XTestFakeButtonEvent.argtypes = [pointer, ctypes.c_uint, integer, window]
            self.connection = self.x.XOpenDisplay(self.name.encode())
            if not self.connection:
                raise RuntimeError("无法连接自有 Xvfb")
            self.root = self.x.XDefaultRootWindow(self.connection)
            self.source = self.x.XCreateSimpleWindow(self.connection, self.root, 0, 0, 1280, 720, 0, 0, 0xffffff)
            self.x.XStoreName(self.connection, self.source, b"Glosslet public probe only")
            self.x.XMapRaised(self.connection, self.source)
            self.x.XSync(self.connection, 0)
            self.prepare()
        except BaseException:
            self.close()
            raise
        finally:
            os.close(reader)
            if writer is not None:
                os.close(writer)

    def prepare(self):
        self.x.XSetInputFocus(self.connection, self.source, 1, 0)
        self.x.XWarpPointer(self.connection, 0, self.root, 0, 0, 0, 0, 500, 500)
        self.x.XSync(self.connection, 0)

    def focus(self):
        window, revert = ctypes.c_ulong(), ctypes.c_int()
        self.x.XGetInputFocus(self.connection, ctypes.byref(window), ctypes.byref(revert))
        return window.value

    def theme(self):
        self.x.XWarpPointer(self.connection, 0, self.root, 0, 0, 0, 0, 650, 530)
        self.x.XSync(self.connection, 0)
        self.xtest.XTestFakeButtonEvent(self.connection, 1, 1, 0)
        self.xtest.XTestFakeButtonEvent(self.connection, 1, 0, 0)
        self.x.XSync(self.connection, 0)

    def close(self):
        if self.connection:
            self.x.XCloseDisplay(self.connection)
            self.connection = None
        stop(self.server)
        self.log.close()


def metrics(pid):
    directory = Path("/proc") / str(pid)
    values = {}
    for line in (directory / "smaps_rollup").read_text().splitlines():
        if line.startswith(("Rss:", "Pss:")):
            key, value, _ = line.split()
            values[key[:-1].lower() + "_kib"] = int(value)
    values["threads"] = sorted(path.joinpath("comm").read_text().strip() for path in (directory / "task").iterdir())
    values["fds"] = len(list((directory / "fd").iterdir()))
    fields = (directory / "stat").read_text().rsplit(")", 1)[1].split()
    values["cpu_ticks"] = int(fields[11]) + int(fields[12])
    return values


def runtime(binary, xvfb, report):
    if os.name != "posix" or not Path("/proc").is_dir():
        raise RuntimeError("runtime 只用于 Linux 自有 Xvfb；没有其他桌面回退")
    samples = {}
    with tempfile.TemporaryDirectory(prefix=NAME + "-acceptance-") as temporary:
        scratch = Path(temporary)
        display = PrivateDisplay(scratch, xvfb)
        processes = []
        runtime_directory = scratch / "runtime"
        runtime_directory.mkdir(mode=0o700)
        environment = {key: value for key, value in os.environ.items() if key not in {
            "WAYLAND_DISPLAY", "WAYLAND_SOCKET", "NIRI_SOCKET", "HYPRLAND_INSTANCE_SIGNATURE", "SWAYSOCK", "XAUTHORITY",
        }}
        environment.update(DISPLAY=display.name, XDG_RUNTIME_DIR=str(runtime_directory), XDG_CONFIG_HOME=str(scratch / "config"), XDG_SESSION_TYPE="x11", XDG_CURRENT_DESKTOP="i3")
        log_path = scratch / "app.log"

        def control(*args, check=True):
            result = subprocess.run([str(binary), *args], env=environment, capture_output=True, text=True, timeout=5)
            if check and result.returncode:
                raise RuntimeError(result.stderr.strip())
            return result

        def status():
            return control("status").stdout

        def settings():
            return json.loads(control("settings").stdout)

        def start():
            with log_path.open("a") as log:
                app = subprocess.Popen([str(binary)], env=environment, stdout=log, stderr=log)
            processes.append(app)

            def ready():
                if app.poll() is not None:
                    raise RuntimeError(log_path.read_text())
                return control("status", check=False).returncode == 0

            wait(ready, "常驻实例未启动")
            return app

        def pin(value):
            action = "pin" if value else "unpin"
            wait(lambda: control(action, check=False).returncode == 0, "当前面板尚未完成绘制")
            control(action)
            assert f"固定：{str(value).lower()}" in status()

        def probe():
            display.prepare()
            control("--probe-panel")
            wait(lambda: "面板：显示中" in status(), "公开面板未显示")
            pin(True)
            pin(False)
            assert display.focus() == display.source, "首次映射夺取了公开来源焦点"

        try:
            config = Path(control("--config-path").stdout.strip())
            assert config == scratch / "config" / NAME / "settings.json"
            assert not config.exists()
            app = start()
            assert settings() == DEFAULT and "阶段：空闲" in status()
            assert control("pin", check=False).returncode != 0
            assert not config.exists(), "查询或固定不应创建配置"
            assert control().returncode == 0, "单实例启动不应再创建常驻进程"
            samples["startup_idle"] = metrics(app.pid)
            for field, value in [("service", "google"), ("direction", "ja"), ("theme", "light")]:
                control("set", field, value)
                assert "阶段：空闲" in status() and "面板：隐藏" in status()
            assert settings() == {"service": "google", "direction": "japanese", "theme": "light"}
            assert stat.S_IMODE(config.stat().st_mode) == 0o600
            assert stat.S_IMODE(config.parent.stat().st_mode) == 0o700
            probe()
            samples["panel_visible"] = metrics(app.pid)
            display.theme()
            wait(lambda: settings()["theme"] == "dark", "真实面板主题按钮未更新偏好")
            assert json.loads(config.read_text()) == settings()
            saved, expected = config.read_text(), settings()
            control("close")
            samples["after_close"] = metrics(app.pid)
            sockets = list(runtime_directory.glob(NAME + "-*.sock"))
            assert len(sockets) == 1
            for data in [b'{"command":"status"}', b"x" * 257 + b"\n"]:
                with socket.socket(socket.AF_UNIX) as stream:
                    stream.settimeout(2)
                    stream.connect(str(sockets[0]))
                    stream.sendall(data)
                    stream.shutdown(socket.SHUT_WR)
                    assert not stream.recv(4096), "不完整或超长帧不能被接受"
                assert "阶段：空闲" in status()
            config.write_text("{")
            probe()
            display.theme()
            time.sleep(0.15)
            assert settings() == expected and "面板：显示中" in status()
            failure = control("set", "theme", "light", check=False)
            assert failure.returncode != 0 and "运行值未改变" in failure.stderr and "拒绝覆盖" in failure.stderr
            assert config.read_text() == "{" and settings() == expected
            control("quit")
            assert app.wait(timeout=5) == 0
            broken = control(check=False)
            assert broken.returncode != 0 and "无法加载设置" in broken.stderr
            assert config.read_text() == "{"
            config.write_text(saved)
            app = start()
            assert settings() == expected and "固定：false" in status() and "面板：隐藏" in status()
            before = metrics(app.pid)
            for _ in range(12):
                probe()
                control("close")
            after = metrics(app.pid)
            samples["after_12_cycles"] = after
            assert len(after["threads"]) == len(before["threads"]), "线程随面板周期累积"
            assert after["fds"] <= before["fds"] + 2, "句柄随面板周期累积"
            assert after["rss_kib"] <= before["rss_kib"] + 8192, "12 次公开面板周期后内存增长超过 8MiB"
            time.sleep(2)
            idle = metrics(app.pid)
            assert idle["cpu_ticks"] - after["cpu_ticks"] <= 2, "空闲持续消耗 CPU"
            samples["post_cycle_idle"] = idle
            control("quit")
            assert app.wait(timeout=5) == 0
            assert list(config.parent.iterdir()) == [config], "遗留临时设置文件"
            samples["runtime_files_after_quit"] = sorted(path.name for path in runtime_directory.iterdir())
            print("私有 Xvfb 验收通过：真实控制、偏好保存、面板按钮、固定、损坏拒绝、重启和资源周期；未读取选区或剪贴板，未请求翻译。")
            print(json.dumps(samples, ensure_ascii=False, indent=2))
        except BaseException:
            if log_path.exists():
                print(log_path.read_text())
            raise
        finally:
            for process in processes:
                if process.poll() is None:
                    control("quit", check=False)
                stop(process)
            display.close()
    report.parent.mkdir(parents=True, exist_ok=True)
    report.write_text(json.dumps(samples, ensure_ascii=False, indent=2) + "\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    formatter = commands.add_parser("fmt")
    formatter.add_argument("--check", action="store_true")
    commands.add_parser("architecture")
    runner = commands.add_parser("runtime")
    runner.add_argument("--binary", type=Path, default=ROOT / "target/release" / NAME)
    runner.add_argument("--xvfb", default="Xvfb")
    runner.add_argument("--report", type=Path, default=ROOT / "target/acceptance/x11.json")
    args = parser.parse_args()
    if args.command == "fmt":
        fmt(args.check)
    elif args.command == "architecture":
        architecture()
    elif args.command == "runtime":
        runtime(args.binary.resolve(), args.xvfb, args.report.resolve())


if __name__ == "__main__":
    main()
