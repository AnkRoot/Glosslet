#!/usr/bin/env python3
"""从 Cargo 与桌面结构定义打包；不安装、不修改用户配置、不发布 AUR。"""

import argparse
import gzip
import hashlib
import io
import json
from pathlib import Path
import shlex
import stat
import subprocess
import tarfile
import tomllib
import zipfile

ROOT = Path(__file__).resolve().parents[1]
PACKAGE = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))["package"]
NAME, VERSION, REPOSITORY = (PACKAGE[key] for key in ("name", "version", "repository"))
PRODUCT = NAME[0].upper() + NAME[1:]
DESKTOPS = json.loads((ROOT / "integration/desktops.json").read_text(encoding="utf-8"))
PLATFORMS = ("linux-x86_64", "linux-aarch64", "windows-x86_64", "macos-x86_64", "macos-aarch64")
MAX_FILE_BYTES = 64 * 1024 * 1024
AUR_ARCHIVE = f"{NAME}-{VERSION}-aur.tar.gz"
BASE_VERSION, _, PRERELEASE = VERSION.partition("-")
ARCH_VERSION = BASE_VERSION + PRERELEASE.replace(".", "")


def zip_bytes(files):
    output = io.BytesIO()
    with zipfile.ZipFile(output, "w", compression=zipfile.ZIP_DEFLATED) as package:
        for name, (data, mode) in sorted(files.items()):
            info = zipfile.ZipInfo(name, (1980, 1, 1, 0, 0, 0))
            info.create_system = 3
            info.external_attr = (stat.S_IFREG | mode) << 16
            info.compress_type = zipfile.ZIP_DEFLATED
            package.writestr(info, data)
    return output.getvalue()


def plugin_files(plugin):
    files = {"metadata.json": ((ROOT / "integration" / plugin["file"]).read_bytes(), 0o644)}
    for destination, source in plugin["files"].items():
        for value in (destination, source):
            path = Path(value)
            if path.is_absolute() or ".." in path.parts:
                raise ValueError("插件归档与来源必须使用受限相对路径")
        files[destination] = ((ROOT / source).read_bytes(), 0o644)
    return files


def desktop(output):
    output.mkdir(parents=True, exist_ok=True)
    for plugin in DESKTOPS["plugins"]:
        if Path(plugin["archive"]).name != plugin["archive"]:
            raise ValueError("插件归档名称必须平铺")
        archive = output / plugin["archive"]
        archive.write_bytes(zip_bytes(plugin_files(plugin)))
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
    if platform not in PLATFORMS or len(data) > MAX_FILE_BYTES:
        raise ValueError("发布平台或程序大小无效")
    arch = platform.rsplit("-", 1)[1]
    if platform.startswith("linux-"):
        if len(data) < 64 or data[:7] != b"\x7fELF\x02\x01\x01":
            raise ValueError("发布程序必须是完整 ELF64 小端二进制")
        if int.from_bytes(data[16:18], "little") not in (2, 3) or int.from_bytes(data[52:54], "little") != 64:
            raise ValueError("ELF 必须是可执行程序或 PIE")
        actual = int.from_bytes(data[18:20], "little")
        expected = {"x86_64": 62, "aarch64": 183}[arch]
    elif platform.startswith("windows-"):
        if len(data) < 64 or data[:2] != b"MZ":
            raise ValueError("发布程序缺少 Windows DOS 头")
        offset = int.from_bytes(data[60:64], "little")
        if offset < 64 or len(data) < offset + 26 or data[offset:offset + 4] != b"PE\0\0":
            raise ValueError("发布程序缺少完整 Windows PE 头")
        optional_size = int.from_bytes(data[offset + 20:offset + 22], "little")
        flags = int.from_bytes(data[offset + 22:offset + 24], "little")
        if optional_size < 112 or len(data) < offset + 24 + optional_size or data[offset + 24:offset + 26] != b"\x0b\x02":
            raise ValueError("Windows 程序必须包含完整 PE32+ 可选头")
        if not flags & 0x0002 or flags & 0x2000:
            raise ValueError("Windows 发布文件必须是可执行程序，不是 DLL")
        actual, expected = int.from_bytes(data[offset + 4:offset + 6], "little"), 0x8664
    else:
        if len(data) < 32 or data[:4] != b"\xcf\xfa\xed\xfe":
            raise ValueError("发布程序必须是完整 Mach-O 64 位小端二进制")
        if int.from_bytes(data[12:16], "little") != 2:
            raise ValueError("Mach-O 发布文件必须是可执行程序")
        actual = int.from_bytes(data[4:8], "little")
        expected = {"x86_64": 0x01000007, "aarch64": 0x0100000c}[arch]
    if actual != expected:
        raise ValueError(f"二进制架构与 {platform} 不符")


def portable_files(platform, data):
    root = f"{NAME}-{VERSION}"
    executable = NAME + (".exe" if platform.startswith("windows-") else "")
    files = {f"{root}/{executable}": (data, 0o755)}
    for filename in ("README.md", "LICENSE"):
        files[f"{root}/{filename}"] = ((ROOT / filename).read_bytes(), 0o644)
    if platform.startswith("linux-"):
        for descriptor in [*DESKTOPS["desktops"], *DESKTOPS["plugins"]]:
            filename = descriptor["file"]
            if Path(filename).name != filename:
                raise ValueError("桌面产物名称必须平铺")
            files[f"{root}/integration/{filename}"] = ((ROOT / "integration" / filename).read_bytes(), 0o644)
        for plugin in DESKTOPS["plugins"]:
            files[f"{root}/integration/{plugin['archive']}"] = (zip_bytes(plugin_files(plugin)), 0o644)
    files[f"{root}/build.json"] = (json.dumps({
        "product": PRODUCT, "version": VERSION, "platform": platform,
        "repository": REPOSITORY, "desktop_support": "experimental", "signed": False,
    }, indent=2).encode() + b"\n", 0o644)
    return files


def portable(platform, binary, output):
    output.mkdir(parents=True, exist_ok=True)
    if not binary.is_file():
        raise FileNotFoundError(binary)
    data = binary.read_bytes()
    verify_binary(data, platform)
    files = portable_files(platform, data)
    archive = output / archive_name(platform)
    if platform.startswith("windows-"):
        archive.write_bytes(zip_bytes(files))
    else:
        tar(archive, files)
    print(archive)
    return archive


def aur(archives, output):
    pkgname = NAME + "-bin"
    pkgver = ARCH_VERSION
    checksums = {}
    for arch in ("x86_64", "aarch64"):
        path = archives / archive_name("linux-" + arch)
        if not path.is_file():
            raise FileNotFoundError(f"AUR 必须使用真实 Linux 发布归档：{path}")
        verify_archive(path, "linux-" + arch)
        checksums[arch] = hashlib.sha256(path.read_bytes()).hexdigest()
    output.mkdir(parents=True, exist_ok=True)
    lines = [
        f"pkgname={pkgname}", f"pkgver={pkgver}", "pkgrel=1",
        f"pkgdesc={shlex.quote(PACKAGE['description'])}", "arch=('x86_64' 'aarch64')",
        f"url={shlex.quote(REPOSITORY)}", f"license=({shlex.quote(PACKAGE['license'])})",
        "depends=('glibc' 'gcc-libs' 'libxkbcommon' 'libxkbcommon-x11' 'libx11' 'libxcb')",
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
    (output / "PKGBUILD").write_text("\n".join(lines), encoding="utf-8")
    # .SRCINFO 必须从真实 PKGBUILD 派生；缺少 makepkg 直接失败，不手工维护第二份结构。
    info = subprocess.check_output(["makepkg", "--printsrcinfo"], cwd=output, encoding="utf-8")
    (output / ".SRCINFO").write_text(info, encoding="utf-8")
    package = archives / AUR_ARCHIVE
    tar(package, {f"{pkgname}/{name}": ((output / name).read_bytes(), 0o644) for name in ("PKGBUILD", ".SRCINFO")})
    print(package)
    print("仅生成 AUR 提交文件；尚未向 AUR 上传。")
    return package


def validate_entries(entries, expected):
    names = [name for name, _, _, _ in entries]
    if len(set(names)) != len(names) or set(names) != set(expected):
        raise ValueError("归档条目重复、缺失或包含未声明文件")
    for name, size, mode, regular in entries:
        if not regular or mode != expected[name][1] or not 0 <= size <= MAX_FILE_BYTES:
            raise ValueError("归档必须是限长、权限正确的普通文件，不能包含链接")


def read_archive(data, expected, kind):
    source = io.BytesIO(data)
    try:
        if kind == "zip":
            with zipfile.ZipFile(source) as package:
                infos = package.infolist()
                validate_entries([
                    (info.filename, info.file_size, stat.S_IMODE(info.external_attr >> 16),
                     info.create_system == 3 and stat.S_ISREG(info.external_attr >> 16) and not info.flag_bits & 1)
                    for info in infos
                ], expected)
                return {info.filename: package.read(info) for info in infos}
        if kind == "tar":
            with tarfile.open(fileobj=source) as package:
                infos = package.getmembers()
                validate_entries([
                    (info.name, info.size, info.mode, info.isfile() and not info.issparse() and info.uid == info.gid == 0)
                    for info in infos
                ], expected)
                return {info.name: package.extractfile(info).read() for info in infos}
    except (zipfile.BadZipFile, tarfile.TarError, EOFError) as error:
        raise ValueError("归档格式损坏或不完整") from error
    raise ValueError("未知归档格式")


def verify_plugin(data, plugin):
    expected = plugin_files(plugin)
    actual = read_archive(data, expected, "zip")
    if any(actual[name] != content for name, (content, _) in expected.items()):
        raise ValueError("插件内容与当前桌面定义不符")


def verify_archive(archive, platform):
    if platform not in PLATFORMS:
        raise ValueError("未知发布平台")
    if not archive.exists():
        raise FileNotFoundError(archive)
    if archive.is_symlink() or not archive.is_file():
        raise ValueError("发布输入必须是普通归档文件")
    expected = portable_files(platform, b"")
    executable = f"{NAME}-{VERSION}/{NAME}" + (".exe" if platform.startswith("windows-") else "")
    actual = read_archive(archive.read_bytes(), expected, "zip" if platform.startswith("windows-") else "tar")
    verify_binary(actual[executable], platform)
    plugins = {}
    if platform.startswith("linux-"):
        plugins = {f"{NAME}-{VERSION}/integration/{plugin['archive']}": plugin for plugin in DESKTOPS["plugins"]}
        for name, plugin in plugins.items():
            verify_plugin(actual[name], plugin)
    if any(actual[name] != data for name, (data, _) in expected.items() if name != executable and name not in plugins):
        raise ValueError("归档说明、构建标识或桌面资产与当前产品定义不符")


def verify_archives(archives):
    for platform in PLATFORMS:
        verify_archive(archives / archive_name(platform), platform)
    print("五种原生归档：架构、精确资产清单、内容、权限、无链接及大小预算验证通过")


def metadata(output):
    verify_archives(output)
    names = [*(archive_name(platform) for platform in PLATFORMS),
             *(plugin["archive"] for plugin in DESKTOPS["plugins"]), AUR_ARCHIVE]
    if len(set(names)) != len(names):
        raise ValueError("发布资产名称重复")
    reserved = {"SHA256SUMS.txt", "RELEASE_NOTES.md", "aur"}
    if any(path.is_symlink() or path.name not in {*names, *reserved} for path in output.iterdir()):
        raise ValueError("发布目录存在未声明文件、目录或链接")
    files = sorted(output / name for name in names)
    if any(not path.is_file() or path.is_symlink() for path in files):
        raise ValueError("发布资产缺失或不是普通文件")
    for plugin in DESKTOPS["plugins"]:
        verify_plugin((output / plugin["archive"]).read_bytes(), plugin)
    aur_files = {f"{NAME}-bin/{name}": (b"", 0o644) for name in ("PKGBUILD", ".SRCINFO")}
    aur_data = read_archive((output / AUR_ARCHIVE).read_bytes(), aur_files, "tar")
    checksums = {path.name: hashlib.sha256(path.read_bytes()).hexdigest() for path in files}
    for arch in ("x86_64", "aarch64"):
        digest = checksums[archive_name("linux-" + arch)].encode()
        if any(digest not in data for data in aur_data.values()):
            raise ValueError("AUR 文件没有使用当前 Linux 归档的校验和")
    checksum_file = output / "SHA256SUMS.txt"
    checksum_file.write_text("".join(f"{digest}  {name}\n" for name, digest in checksums.items()), encoding="utf-8")
    (output / "RELEASE_NOTES.md").write_text(
        f"{PRODUCT} {VERSION} 原生实验版：归档解压即可运行，配置仍在系统标准目录。\n\n"
        "平台支持以 README 的真实桌面验收为准；CI 原生构建不等于真机验收。\n"
        "Windows/macOS 尚未完成发行签名；Niri/Sway 定位采样仍有短暂输入损失风险。\n"
        "真实查词只向所选服务发送选区，不自动换服务；请勿翻译敏感文档。\n"
        "独立 GNOME/KWin 插件与 AUR 提交文件已附带；尚未上传 AUR，不自动部署桌面配置。\n",
        encoding="utf-8",
    )
    return [*files, checksum_file]


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
    info = commands.add_parser("metadata")
    info.add_argument("--output", type=Path, default=ROOT / "target/package")
    info.add_argument("--github-output", type=Path, required=True)
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
    elif args.command == "metadata":
        files = metadata(args.output.resolve())
        with args.github_output.open("a", encoding="utf-8") as stream:
            stream.write("prerelease=" + str(bool(PRERELEASE)).lower() + "\n")
            stream.write("files<<GLOSSLET_ASSETS\n" + "".join(str(path) + "\n" for path in files) + "GLOSSLET_ASSETS\n")


if __name__ == "__main__":
    main()
