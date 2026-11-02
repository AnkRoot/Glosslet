"""归档与 AUR 结构测试使用自有合成程序头，不安装或执行它们。"""

import hashlib
import importlib.util
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest
import zipfile

spec = importlib.util.spec_from_file_location("packaging", Path(__file__).resolve().parents[1] / "release/package.py")
packaging = importlib.util.module_from_spec(spec)
spec.loader.exec_module(packaging)


def synthetic_binary(platform):
    data = bytearray(128)
    if platform.startswith("linux-"):
        data[:6] = b"\x7fELF\x02\x01"
        data[18:20] = (62 if platform.endswith("x86_64") else 183).to_bytes(2, "little")
    elif platform.startswith("windows-"):
        data[:2] = b"MZ"
        data[60:64] = (64).to_bytes(4, "little")
        data[64:68] = b"PE\0\0"
        data[68:70] = (0x8664).to_bytes(2, "little")
    else:
        data[:4] = b"\xcf\xfa\xed\xfe"
        data[4:8] = (0x01000007 if platform.endswith("x86_64") else 0x0100000c).to_bytes(4, "little")
    return bytes(data)


class ReleaseTests(unittest.TestCase):
    def test_program_architecture_must_match_not_only_the_filename(self):
        for platform in packaging.PLATFORMS:
            packaging.verify_binary(synthetic_binary(platform), platform)
            with self.assertRaises(ValueError):
                packaging.verify_binary(b"not a program", platform)
        with self.assertRaises(ValueError):
            packaging.verify_binary(synthetic_binary("linux-x86_64"), "linux-aarch64")
        with self.assertRaises(ValueError):
            packaging.verify_binary(synthetic_binary("macos-x86_64"), "macos-aarch64")

    def test_plugins_and_portable_archives_contain_only_declared_assets(self):
        with tempfile.TemporaryDirectory(prefix=packaging.NAME + "-release-test-") as temporary:
            output = Path(temporary)
            for platform in packaging.PLATFORMS:
                binary = output / "synthetic"
                binary.write_bytes(synthetic_binary(platform))
                packaging.portable(platform, binary, output)
            packaging.verify_archives(output)
            for plugin in packaging.DESKTOPS["plugins"]:
                with zipfile.ZipFile(output / "plugins" / plugin["archive"]) as archive:
                    self.assertEqual(set(archive.namelist()), {"metadata.json", *plugin["files"]})
            with tarfile.open(output / packaging.archive_name("linux-x86_64")) as archive:
                executable = archive.getmember(f"{packaging.NAME}-{packaging.VERSION}/{packaging.NAME}")
                self.assertEqual(executable.mode, 0o755)
                self.assertTrue(all(member.uid == 0 and member.mtime == 0 for member in archive))

    def test_aur_uses_actual_archives_and_srcinfo_is_derived_by_makepkg(self):
        if not Path("/usr/bin/makepkg").is_file():
            self.skipTest("AUR .SRCINFO 集成验收需要 Arch makepkg；其他平台只验证归档")
        with tempfile.TemporaryDirectory(prefix=packaging.NAME + "-aur-test-") as temporary:
            output = Path(temporary)
            for arch in ("x86_64", "aarch64"):
                binary = output / "synthetic"
                binary.write_bytes(synthetic_binary("linux-" + arch))
                packaging.portable("linux-" + arch, binary, output)
            aur = output / "aur"
            package = packaging.aur(output, aur)
            text = (aur / "PKGBUILD").read_text(encoding="utf-8")
            self.assertNotIn("SKIP", text)
            for arch in ("x86_64", "aarch64"):
                digest = hashlib.sha256((output / packaging.archive_name("linux-" + arch)).read_bytes()).hexdigest()
                self.assertIn(digest, text)
            info = subprocess.check_output(["makepkg", "--printsrcinfo"], cwd=aur, encoding="utf-8")
            self.assertEqual((aur / ".SRCINFO").read_text(encoding="utf-8"), info)
            self.assertIn("depends = libxkbcommon-x11", info)
            self.assertNotRegex(info, r"\btran\b")
            with tarfile.open(package) as archive:
                self.assertEqual(set(archive.getnames()), {f"{packaging.NAME}-bin/PKGBUILD", f"{packaging.NAME}-bin/.SRCINFO"})
            (output / packaging.archive_name("linux-aarch64")).unlink()
            with self.assertRaises(FileNotFoundError):
                packaging.aur(output, aur)


if __name__ == "__main__":
    unittest.main()
