import importlib.util
from pathlib import Path
import plistlib
import tarfile
import tempfile
import unittest
from unittest.mock import patch
import zipfile

spec = importlib.util.spec_from_file_location("package", Path(__file__).with_name("package.py"))
package = importlib.util.module_from_spec(spec)
spec.loader.exec_module(package)


class PackagingTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        for name in ("eagle-desktop", "eagle-cli", "eagle-desktop.exe", "eagle-cli.exe"):
            (self.bin / name).write_bytes(b"binary")
        for name in ("LICENSE", "EXCEPTIONS.md", "assets/brand/out/desktop/eagle.icns"):
            path = self.root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"asset")
        self.root_patch = patch.object(package, "ROOT", self.root)
        self.root_patch.start()
        self.addCleanup(self.root_patch.stop)

    def build(self, target):
        return package.package(target, "1.2.3", self.bin, self.root / "out")

    def test_linux_archive_modes(self):
        with tarfile.open(self.build("x86_64-unknown-linux-gnu")) as tar:
            self.assertEqual(set(tar.getnames()), {"eagle-desktop", "eagle-cli", "LICENSE", "EXCEPTIONS.md"})
            self.assertEqual(tar.getmember("eagle-desktop").mode & 0o777, 0o755)
            self.assertEqual(tar.getmember("eagle-cli").mode & 0o777, 0o755)

    def test_windows_zip(self):
        with zipfile.ZipFile(self.build("x86_64-pc-windows-msvc")) as zf:
            self.assertEqual(set(zf.namelist()), {"eagle-desktop.exe", "eagle-cli.exe", "LICENSE", "EXCEPTIONS.md"})

    def test_mac_bundle(self):
        for target in ("aarch64-apple-darwin", "x86_64-apple-darwin"):
            with self.subTest(target=target), zipfile.ZipFile(self.build(target)) as zf:
                info = plistlib.loads(zf.read("Eagle.app/Contents/Info.plist"))
                self.assertEqual(info["CFBundleShortVersionString"], "1.2.3")
                self.assertEqual(info["CFBundleExecutable"], "eagle-desktop")
                self.assertIn("Eagle.app/Contents/Resources/eagle.icns", zf.namelist())
                self.assertIn("EXCEPTIONS.md", zf.namelist())
                for name in ("Eagle.app/Contents/MacOS/eagle-desktop", "eagle-cli"):
                    self.assertEqual((zf.getinfo(name).external_attr >> 16) & 0o777, 0o755)

    def test_mac_prerelease_metadata(self):
        archive = package.package("aarch64-apple-darwin", "1.2.3-rc.1+build.4", self.bin, self.root / "out")
        with zipfile.ZipFile(archive) as zf:
            info = plistlib.loads(zf.read("Eagle.app/Contents/Info.plist"))
            self.assertEqual(info["CFBundleShortVersionString"], "1.2.3")
            self.assertEqual(info["CFBundleVersion"], "1.2.3")
            self.assertEqual(info["CFBundleGetInfoString"], "1.2.3-rc.1+build.4")

    def test_rejects_invalid_input(self):
        for target, version in (("bad", "1.2.3"), ("x86_64-unknown-linux-gnu", "../1.2.3"), ("x86_64-unknown-linux-gnu", "1.2.3-01")):
            with self.subTest(target=target, version=version), self.assertRaises(ValueError):
                package.package(target, version, self.bin, self.root / "out")


if __name__ == "__main__":
    unittest.main()
