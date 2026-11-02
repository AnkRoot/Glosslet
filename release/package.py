#!/usr/bin/env python3
"""从 Cargo 与桌面结构定义打包；不安装、不修改用户配置、不发布 AUR。"""

import argparse
import gzip
import hashlib
import io
import json
from pathlib import Path
import shlex
import subprocess
import tarfile
import tomllib
import zipfile

ROOT = Path(__file__).resolve().parents[1]
PACKAGE = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]
NAME, VERSION, REPOSITORY = (PACKAGE[key] for key in ("name", "version", "repository"))
PRODUCT = NAME[0].upper() + NAME[1:]
DESKTOPS = json.loads((ROOT / "integration/desktops.json").read_text())
PLATFORMS = ("linux-x86_64", "linux-aarch64", "windows-x86_64", "macos-x86_64", "macos-aarch64")


def desktop(output):
    output.mkdir(parents=True, exist_ok=True)
    for plugin in DESKTOPS["plugins"]:
        archive = output / plugin["archive"]
        with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_DEFLATED) as package:
            package.write(ROOT / "integration" / plugin["file"], "metadata.json")
            for destination, source in plugin["files"].items():
                path = Path(destination)
                if path.is_absolute() or ".." in path.parts:
                    raise ValueError("插件归档必须使用受限相对路径")
                package.write(ROOT / source, destination)
        print(archive)


def archive_name(platform):
    return f"{NAME}-{VERSION}-{platform}" + (".zip" if platform.startswith("windows-") else ".tar.gz")


def tar(output, files):
    """文件顺序、所有者、时间和权限确定；校验和只取实际归档，不使用 SKIP。"""
    with output.open("wb") as raw:
        with gzip.GzipFile(fileobj=raw, mode="wb", filename="", mtime=0) as compressed:
            with tarfile.open(fileobj=compressed, mode="w") as package:
                for name, (data, mode) in sorted(files.items()):
                    info = tarfile.TarInfo(name)
                    info.size, info.mode, info.mtime = len(data), mode, 0
                    package.addfile(info, io.BytesIO(data))


def verify_binary(data, platform):
    arch = platform.rsplit("-", 1)[1]
    if platform.startswith("linux-"):
        if len(data) < 20 or data[:4] != b"\x7fELF" or data[4] != 2 or data[5] != 1:
            raise ValueError("发布程序必须是 ELF64 小端二进制")
        actual = int.from_bytes(data[18:20], "little")
        expected = {"x86_64": 62, "aarch64": 183}[arch]
    elif platform.startswith("windows-"):
        if len(data) < 64 or data[:2] != b"MZ":
            raise ValueError("发布程序缺少 Windows DOS 头")
        offset = int.from_bytes(data[60:64], "little")
        if data[offset:offset + 4] != b"PE\0\0":
            raise ValueError("发布程序缺少 Windows PE 头")
        actual, expected = int.from_bytes(data[offset + 4:offset + 6], "little"), 0x8664
    else:
        if len(data) < 8 or data[:4] != b"\xcf\xfa\xed\xfe":
            raise ValueError("发布程序必须是原生 Mach-O 64 位小端二进制")
        actual = int.from_bytes(data[4:8], "little")
        expected = {"x86_64": 0x01000007, "aarch64": 0x0100000c}[arch]
    if actual != expected:
        raise ValueError(f"二进制架构与 {platform} 不符")


def portable(platform, binary, output):
    output.mkdir(parents=True, exist_ok=True)
    if not binary.is_file():
        raise FileNotFoundError(binary)
    plugins = output / "plugins"
    if platform.startswith("linux-"):
        desktop(plugins)
    root = f"{NAME}-{VERSION}"
    executable = NAME + (".exe" if platform.startswith("windows-") else "")
    data = binary.read_bytes()
    verify_binary(data, platform)
    files = {f"{root}/{executable}": (data, 0o755)}
    for filename in ("README.md", "LICENSE"):
        files[f"{root}/{filename}"] = ((ROOT / filename).read_bytes(), 0o644)
    if platform.startswith("linux-"):
        for path in sorted((ROOT / "integration").iterdir()):
            if path.name != "desktops.json":
                files[f"{root}/integration/{path.name}"] = (path.read_bytes(), 0o644)
        for plugin in DESKTOPS["plugins"]:
            files[f"{root}/integration/{plugin['archive']}"] = ((plugins / plugin["archive"]).read_bytes(), 0o644)
    files[f"{root}/build.json"] = (json.dumps({
        "product": PRODUCT, "version": VERSION, "platform": platform,
        "repository": REPOSITORY, "desktop_support": "experimental", "signed": False,
    }, indent=2).encode() + b"\n", 0o644)
    archive = output / archive_name(platform)
    if platform.startswith("windows-"):
        with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_DEFLATED) as package:
            for name, (data, mode) in sorted(files.items()):
                info = zipfile.ZipInfo(name, (1980, 1, 1, 0, 0, 0))
                info.external_attr, info.compress_type = mode << 16, zipfile.ZIP_DEFLATED
                package.writestr(info, data)
    else:
        tar(archive, files)
    print(archive)
    return archive


def aur(archives, output):
    pkgname = NAME + "-bin"
    pkgver = VERSION.replace("-", ".")
    checksums = {}
    for arch in ("x86_64", "aarch64"):
        path = archives / archive_name("linux-" + arch)
        if not path.is_file():
            raise FileNotFoundError(f"AUR 必须使用真实 Linux 发布归档：{path}")
        checksums[arch] = hashlib.sha256(path.read_bytes()).hexdigest()
    output.mkdir(parents=True, exist_ok=True)
    lines = [
        f"pkgname={pkgname}", f"pkgver={pkgver}", "pkgrel=1",
        f"pkgdesc={shlex.quote(PACKAGE['description'])}", "arch=('x86_64' 'aarch64')",
        f"url={shlex.quote(REPOSITORY)}", f"license=({shlex.quote(PACKAGE['license'])})",
        "depends=('glibc' 'gcc-libs' 'libxkbcommon' 'libx11' 'libxcb')",
        "optdepends=('noto-fonts-cjk: 中文和日文字体')",
        f"provides=('{NAME}')", f"conflicts=('{NAME}')", "options=('!strip')", "",
        f"_release={shlex.quote(VERSION)}",
    ]
    for arch, checksum in checksums.items():
        filename = archive_name("linux-" + arch)
        lines.extend([
            f"source_{arch}=(\"$url/releases/download/v$_release/{filename}\")",
            f"sha256sums_{arch}=('{checksum}')",
        ])
    lines.extend([
        "", "package() {", f'    cd "$srcdir/{NAME}-$_release"',
        f'    install -Dm755 {NAME} "$pkgdir/usr/bin/{NAME}"',
        f'    install -Dm644 LICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE"',
        f'    install -Dm644 README.md "$pkgdir/usr/share/doc/{NAME}/README.md"',
        '    for file in integration/*; do',
        f'        install -Dm644 "$file" "$pkgdir/usr/share/{NAME}/integration/${{file##*/}}"',
        "    done", "}", "",
    ])
    (output / "PKGBUILD").write_text("\n".join(lines))
    # .SRCINFO 必须从真实 PKGBUILD 派生；缺少 makepkg 直接失败，不手工维护第二份结构。
    info = subprocess.check_output(["makepkg", "--printsrcinfo"], cwd=output, text=True)
    (output / ".SRCINFO").write_text(info)
    package = archives / f"{NAME}-{VERSION}-aur.tar.gz"
    tar(package, {f"{pkgname}/{name}": ((output / name).read_bytes(), 0o644) for name in ("PKGBUILD", ".SRCINFO")})
    print(package)
    print("仅生成 AUR 提交文件；尚未向 AUR 上传。")
    return package


def verify_archives(archives):
    for platform in PLATFORMS:
        archive = archives / archive_name(platform)
        if not archive.is_file():
            raise FileNotFoundError(archive)
        executable = NAME + (".exe" if platform.startswith("windows-") else "")
        root = f"{NAME}-{VERSION}"
        if platform.startswith("windows-"):
            with zipfile.ZipFile(archive) as package:
                names = package.namelist()
                manifest = json.loads(package.read(f"{root}/build.json"))
                verify_binary(package.read(f"{root}/{executable}"), platform)
        else:
            with tarfile.open(archive) as package:
                names = package.getnames()
                manifest = json.load(package.extractfile(f"{root}/build.json"))
                verify_binary(package.extractfile(f"{root}/{executable}").read(), platform)
        assert len(set(names)) == len(names), "归档条目重复"
        assert {f"{root}/{executable}", f"{root}/README.md", f"{root}/LICENSE"} <= set(names)
        assert manifest["platform"] == platform and manifest["version"] == VERSION
        assert all(name.startswith(root + "/") and ".." not in Path(name).parts for name in names)
        assert not any("settings.json" in name or name.endswith((".sock", ".lock")) for name in names)
    print("五种原生归档结构、平台标识、程序格式与无用户数据夹带：通过")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    plugins = commands.add_parser("desktop")
    plugins.add_argument("--output", type=Path, default=ROOT / "target/package")
    bundle = commands.add_parser("portable")
    bundle.add_argument("platform", choices=PLATFORMS)
    bundle.add_argument("--binary", type=Path)
    bundle.add_argument("--output", type=Path, default=ROOT / "target/package")
    arch = commands.add_parser("aur")
    arch.add_argument("--archives", type=Path, default=ROOT / "target/package")
    arch.add_argument("--output", type=Path, default=ROOT / "target/package/aur")
    check = commands.add_parser("check")
    check.add_argument("--archives", type=Path, default=ROOT / "target/package")
    args = parser.parse_args()
    if args.command == "desktop":
        desktop(args.output.resolve())
    elif args.command == "portable":
        binary = args.binary or ROOT / "target/release" / (NAME + (".exe" if args.platform.startswith("windows-") else ""))
        portable(args.platform, binary.resolve(), args.output.resolve())
    elif args.command == "aur":
        aur(args.archives.resolve(), args.output.resolve())
    elif args.command == "check":
        verify_archives(args.archives.resolve())


if __name__ == "__main__":
    main()
