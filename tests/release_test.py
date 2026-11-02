"""归档测试只用自有合成程序头／提交文件；不安装或执行它们。

Arch 上另用真实 makepkg 派生 .SRCINFO；合成样本不代表已生成真实发行二进制。
"""

from contextlib import redirect_stdout
import gzip
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import stat
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch
import zipfile

spec = importlib.util.spec_from_file_location("packaging", Path(__file__).resolve().parents[1] / "release/package.py")
packaging = importlib.util.module_from_spec(spec)
spec.loader.exec_module(packaging)


def synthetic_binary(platform):
    data = bytearray(256)
    if platform.startswith("linux-"):
        data[:7] = b"\x7fELF\x02\x01\x01"
        data[16:18] = (3).to_bytes(2, "little")
        data[18:20] = (62 if platform.endswith("x86_64") else 183).to_bytes(2, "little")
        data[20:24] = (1).to_bytes(4, "little")
        data[52:54] = (64).to_bytes(2, "little")
    elif platform.startswith("windows-"):
        data[:2] = b"MZ"
        data[60:64] = (64).to_bytes(4, "little")
        data[64:68] = b"PE\0\0"
        data[68:70] = (0x8664).to_bytes(2, "little")
        data[84:86] = (112).to_bytes(2, "little")
        data[86:88] = (2).to_bytes(2, "little")
        data[88:90] = (0x020b).to_bytes(2, "little")
    else:
        data[:4] = b"\xcf\xfa\xed\xfe"
        data[4:8] = (0x01000007 if platform.endswith("x86_64") else 0x0100000c).to_bytes(4, "little")
        data[12:16] = (2).to_bytes(4, "little")
    return bytes(data)


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        scratch = tempfile.TemporaryDirectory(prefix=packaging.NAME + "-release-test-")
        self.addCleanup(scratch.cleanup)
        self.scratch = Path(scratch.name).resolve()
        self.output = self.scratch / "archives"
        self.output.mkdir()

    def portables(self, platforms=packaging.PLATFORMS):
        binary = self.scratch / "synthetic"
        with redirect_stdout(io.StringIO()):
            for platform in platforms:
                binary.write_bytes(synthetic_binary(platform))
                packaging.portable(platform, binary, self.output)

    def rewrite(self, platform, files):
        path = self.output / packaging.archive_name(platform)
        if platform.startswith("windows-"):
            path.write_bytes(packaging.zip_bytes(files))
        else:
            packaging.tar(path, files)
        return path

    def release_files(self):
        self.portables()
        with redirect_stdout(io.StringIO()):
            packaging.desktop(self.output)
        digests = "\n".join(
            hashlib.sha256((self.output / packaging.archive_name("linux-" + arch)).read_bytes()).hexdigest()
            for arch in ("x86_64", "aarch64")
        ).encode()
        # 这里只合成 AUR 清单与校验和；真实 makepkg 的验证在独立测试和发行 CI 中。
        packaging.tar(self.output / packaging.AUR_ARCHIVE, {
            f"{packaging.NAME}-bin/{name}": (digests, 0o644)
            for name in ("PKGBUILD", ".SRCINFO")
        })

    def test_program_architecture_and_executable_type_must_match(self):
        for platform in packaging.PLATFORMS:
            data = synthetic_binary(platform)
            packaging.verify_binary(data, platform)
            for truncated in (b"not a program", data[:20]):
                with self.subTest(platform=platform, truncated=len(truncated)), self.assertRaises(ValueError):
                    packaging.verify_binary(truncated, platform)
            changed = bytearray(data)
            if platform.startswith("linux-"):
                changed[16:18] = (1).to_bytes(2, "little")
            elif platform.startswith("windows-"):
                changed[86:88] = (0x2002).to_bytes(2, "little")
            else:
                changed[12:16] = (6).to_bytes(4, "little")
            with self.subTest(platform=platform, wrong_type=True), self.assertRaises(ValueError):
                packaging.verify_binary(changed, platform)
        for source, destination in [("linux-x86_64", "linux-aarch64"), ("macos-x86_64", "macos-aarch64")]:
            with self.assertRaises(ValueError):
                packaging.verify_binary(synthetic_binary(source), destination)
        data = bytearray(synthetic_binary("windows-x86_64"))
        data[60:64] = (252).to_bytes(4, "little")
        with self.assertRaises(ValueError):
            packaging.verify_binary(data, "windows-x86_64")
        with self.assertRaises(ValueError):
            packaging.verify_binary(b"", "unknown")

    def test_plugins_and_portables_have_declared_content_and_permissions(self):
        self.portables()
        packaging.verify_archives(self.output)
        with redirect_stdout(io.StringIO()):
            packaging.desktop(self.output)
        for plugin in packaging.DESKTOPS["plugins"]:
            with zipfile.ZipFile(self.output / plugin["archive"]) as archive:
                self.assertEqual(set(archive.namelist()), {"metadata.json", *plugin["files"]})
                for member in archive.infolist():
                    self.assertEqual(member.date_time, (1980, 1, 1, 0, 0, 0))
                    self.assertTrue(stat.S_ISREG(member.external_attr >> 16))
        with tarfile.open(self.output / packaging.archive_name("linux-x86_64")) as archive:
            executable = archive.getmember(f"{packaging.NAME}-{packaging.VERSION}/{packaging.NAME}")
            self.assertEqual(executable.mode, 0o755)
            self.assertTrue(all(member.uid == member.gid == member.mtime == 0 for member in archive))

    def test_plugin_bytes_do_not_depend_on_source_mtime(self):
        shadow = self.scratch / "sources"
        sources = set()
        for plugin in packaging.DESKTOPS["plugins"]:
            sources.add("integration/" + plugin["file"])
            sources.update(plugin["files"].values())
        for relative in sources:
            path = shadow / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes((packaging.ROOT / relative).read_bytes())
        with patch.object(packaging, "ROOT", shadow), redirect_stdout(io.StringIO()):
            packaging.desktop(self.output)
            before = {plugin["archive"]: (self.output / plugin["archive"]).read_bytes() for plugin in packaging.DESKTOPS["plugins"]}
            for relative in sources:
                os.utime(shadow / relative, (1000000000, 1000000000))
            packaging.desktop(self.output)
            self.assertEqual(before, {name: (self.output / name).read_bytes() for name in before})

    def test_archive_rejects_undeclared_paths_missing_assets_and_wrong_modes(self):
        for platform in ("linux-x86_64", "windows-x86_64"):
            original = packaging.portable_files(platform, synthetic_binary(platform))
            root = f"{packaging.NAME}-{packaging.VERSION}"
            executable = f"{root}/{packaging.NAME}" + (".exe" if platform.startswith("windows-") else "")
            for extra in (f"{root}/settings.json", f"{root}/../escape", "/absolute", f"{root}/private.txt"):
                files = {**original, extra: (b"public fixture", 0o644)}
                path = self.rewrite(platform, files)
                with self.subTest(platform=platform, extra=extra), self.assertRaises(ValueError):
                    packaging.verify_archive(path, platform)
            files = dict(original)
            files.pop(f"{root}/LICENSE")
            with self.assertRaises(ValueError):
                packaging.verify_archive(self.rewrite(platform, files), platform)
            files = {**original, executable: (synthetic_binary(platform), 0o644)}
            with self.assertRaises(ValueError):
                packaging.verify_archive(self.rewrite(platform, files), platform)

    def test_archive_build_identity_is_closed_and_matches_current_source(self):
        platform = "linux-x86_64"
        original = packaging.portable_files(platform, synthetic_binary(platform))
        key = f"{packaging.NAME}-{packaging.VERSION}/build.json"
        for field, value in [("product", "Other"), ("repository", "https://example.invalid"), ("version", "0"), ("signed", True), ("unknown", True)]:
            manifest = json.loads(original[key][0])
            manifest[field] = value
            files = {**original, key: (json.dumps(manifest).encode(), 0o644)}
            with self.subTest(field=field), self.assertRaises(ValueError):
                packaging.verify_archive(self.rewrite(platform, files), platform)

    def test_archive_rejects_duplicates_links_and_oversized_entries(self):
        for platform in ("linux-x86_64", "windows-x86_64"):
            files = packaging.portable_files(platform, synthetic_binary(platform))
            path = self.rewrite(platform, files)
            if platform.startswith("windows-"):
                output = io.BytesIO()
                with zipfile.ZipFile(output, "w") as archive:
                    for name, (data, mode) in files.items():
                        info = zipfile.ZipInfo(name)
                        info.create_system = 3
                        info.external_attr = (stat.S_IFLNK | mode) << 16
                        archive.writestr(info, data)
                path.write_bytes(output.getvalue())
            else:
                output = io.BytesIO()
                with tarfile.open(fileobj=output, mode="w") as archive:
                    for name, (data, mode) in files.items():
                        info = tarfile.TarInfo(name)
                        info.mode, info.type, info.linkname = mode, tarfile.SYMTYPE, "public-fixture"
                        archive.addfile(info)
                path.write_bytes(gzip.compress(output.getvalue()))
            with self.assertRaises(ValueError):
                packaging.verify_archive(path, platform)
        expected = {"file": (b"", 0o644)}
        for entries in [[("file", 0, 0o644, True)] * 2, [("file", packaging.MAX_FILE_BYTES + 1, 0o644, True)]]:
            with self.assertRaises(ValueError):
                packaging.validate_entries(entries, expected)

    def test_archive_validation_stays_enabled_under_python_optimization(self):
        self.portables()
        platform = "linux-x86_64"
        files = packaging.portable_files(platform, synthetic_binary(platform))
        key = f"{packaging.NAME}-{packaging.VERSION}/build.json"
        files[key] = (b"{}", 0o644)
        self.rewrite(platform, files)
        result = subprocess.run([sys.executable, "-O", str(packaging.ROOT / "release/package.py"), "check", "--archives", str(self.output)], capture_output=True)
        self.assertNotEqual(result.returncode, 0)

    def test_aur_rejects_wrong_architecture_before_calling_makepkg(self):
        self.portables(("linux-x86_64", "linux-aarch64"))
        files = packaging.portable_files("linux-x86_64", synthetic_binary("linux-aarch64"))
        self.rewrite("linux-x86_64", files)
        with patch.object(packaging.subprocess, "check_output", side_effect=AssertionError("不应调用 makepkg")), self.assertRaises(ValueError):
            packaging.aur(self.output, self.output / "aur")

    def test_metadata_uses_one_complete_asset_list_for_checksums_and_upload(self):
        self.release_files()
        with redirect_stdout(io.StringIO()):
            files = packaging.metadata(self.output)
        self.assertEqual(len(files), len(packaging.PLATFORMS) + len(packaging.DESKTOPS["plugins"]) + 2)
        checksums = dict(line.split("  ", 1)[::-1] for line in (self.output / "SHA256SUMS.txt").read_text(encoding="utf-8").splitlines())
        self.assertEqual(set(checksums), {path.name for path in files if path.name != "SHA256SUMS.txt"})
        for name, digest in checksums.items():
            self.assertEqual(digest, hashlib.sha256((self.output / name).read_bytes()).hexdigest())
        self.assertTrue(any(path.suffix == ".kwinscript" for path in files))
        github_output = self.scratch / "github-output"
        result = subprocess.run([sys.executable, str(packaging.ROOT / "release/package.py"), "metadata", "--output", str(self.output), "--github-output", str(github_output)], capture_output=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        lines = github_output.read_text(encoding="utf-8").splitlines()
        self.assertEqual(lines[0], "prerelease=" + str(bool(packaging.PRERELEASE)).lower())
        self.assertEqual(lines[2:-1], list(map(str, files)))

    def test_metadata_rejects_missing_plugins_stale_aur_and_undeclared_files(self):
        self.release_files()
        extra = self.output / "settings.json"
        extra.write_bytes(b"public fixture")
        with redirect_stdout(io.StringIO()), self.assertRaises(ValueError):
            packaging.metadata(self.output)
        extra.unlink()
        plugin = self.output / packaging.DESKTOPS["plugins"][-1]["archive"]
        saved = plugin.read_bytes()
        plugin.unlink()
        with redirect_stdout(io.StringIO()), self.assertRaises(ValueError):
            packaging.metadata(self.output)
        plugin.write_bytes(saved)
        packaging.tar(self.output / packaging.AUR_ARCHIVE, {f"{packaging.NAME}-bin/{name}": (b"wrong checksum", 0o644) for name in ("PKGBUILD", ".SRCINFO")})
        with redirect_stdout(io.StringIO()), self.assertRaises(ValueError):
            packaging.metadata(self.output)

    def test_aur_uses_actual_archives_and_srcinfo_is_derived_by_makepkg(self):
        if not Path("/usr/bin/makepkg").is_file():
            self.skipTest("真实 .SRCINFO 集成验收需要 Arch makepkg")
        self.portables(("linux-x86_64", "linux-aarch64"))
        aur = self.output / "aur"
        with redirect_stdout(io.StringIO()):
            package = packaging.aur(self.output, aur)
        text = (aur / "PKGBUILD").read_text(encoding="utf-8")
        self.assertNotIn("SKIP", text)
        self.assertIn("pkgver=" + packaging.ARCH_VERSION, text)
        for arch in ("x86_64", "aarch64"):
            digest = hashlib.sha256((self.output / packaging.archive_name("linux-" + arch)).read_bytes()).hexdigest()
            self.assertIn(digest, text)
        info = subprocess.check_output(["makepkg", "--printsrcinfo"], cwd=aur, encoding="utf-8")
        self.assertEqual((aur / ".SRCINFO").read_text(encoding="utf-8"), info)
        self.assertIn("depends = libxkbcommon-x11", info)
        self.assertNotRegex(info, r"\btran\b")
        if Path("/usr/bin/vercmp").is_file() and packaging.PRERELEASE:
            self.assertEqual(subprocess.check_output(["vercmp", packaging.ARCH_VERSION, packaging.BASE_VERSION], encoding="utf-8").strip(), "-1")
        with tarfile.open(package) as archive:
            self.assertEqual(set(archive.getnames()), {f"{packaging.NAME}-bin/{name}" for name in ("PKGBUILD", ".SRCINFO")})
        (self.output / packaging.archive_name("linux-aarch64")).unlink()
        with self.assertRaises(FileNotFoundError):
            packaging.aur(self.output, aur)


if __name__ == "__main__":
    unittest.main()
