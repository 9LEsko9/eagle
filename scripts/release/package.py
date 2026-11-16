#!/usr/bin/env python3
"""Package already-built Eagle desktop and CLI binaries."""

import argparse
from pathlib import Path
import plistlib
import re
import shutil
import stat
import tarfile
import tempfile
import zipfile

ROOT = Path(__file__).resolve().parents[2]
TARGETS = {
    "x86_64-pc-windows-msvc": "windows",
    "x86_64-unknown-linux-gnu": "linux",
    "aarch64-apple-darwin": "macos",
    "x86_64-apple-darwin": "macos",
}
VERSION = re.compile(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?(?:\+([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?\Z")


def package(target: str, version: str, bin_dir: Path, output: Path) -> Path:
    if target not in TARGETS:
        raise ValueError(f"unsupported target: {target}")
    match = VERSION.fullmatch(version)
    if not match or (match.group(4) and any(part.isdigit() and len(part) > 1 and part[0] == "0" for part in match.group(4).split("."))):
        raise ValueError(f"invalid semantic version: {version}")
    platform = TARGETS[target]
    extension = ".exe" if platform == "windows" else ""
    output.mkdir(parents=True, exist_ok=True)
    suffix = ".tar.gz" if platform == "linux" else ".zip"
    archive = output / f"eagle-{version}-{target}{suffix}"
    with tempfile.TemporaryDirectory() as temp:
        stage = Path(temp)
        for name in ("eagle-desktop", "eagle-cli"):
            destination = stage / (name + extension)
            shutil.copyfile(bin_dir / (name + extension), destination)
        for name in ("LICENSE", "EXCEPTIONS.md"):
            shutil.copyfile(ROOT / name, stage / name)
        if platform == "macos":
            contents = stage / "Eagle.app" / "Contents"
            (contents / "MacOS").mkdir(parents=True)
            (contents / "Resources").mkdir()
            shutil.move(stage / "eagle-desktop", contents / "MacOS" / "eagle-desktop")
            shutil.copyfile(ROOT / "assets/brand/out/desktop/eagle.icns", contents / "Resources/eagle.icns")
            info = {
                "CFBundleDevelopmentRegion": "en",
                "CFBundleExecutable": "eagle-desktop",
                "CFBundleIconFile": "eagle.icns",
                "CFBundleIdentifier": "app.eagle.music",
                "CFBundleInfoDictionaryVersion": "6.0",
                "CFBundleName": "Eagle",
                "CFBundlePackageType": "APPL",
                "CFBundleShortVersionString": ".".join(match.group(i) for i in range(1, 4)),
                "CFBundleGetInfoString": version,
                "CFBundleVersion": ".".join(match.group(i) for i in range(1, 4)),
                "NSHighResolutionCapable": True,
            }
            with (contents / "Info.plist").open("wb") as f:
                plistlib.dump(info, f)
        if platform == "linux":
            with tarfile.open(archive, "w:gz") as tar:
                for path in sorted(stage.iterdir()):
                    info = tar.gettarinfo(path, arcname=path.name)
                    info.mode = 0o755 if path.name in ("eagle-desktop", "eagle-cli") else 0o644
                    with path.open("rb") as source:
                        tar.addfile(info, source)
        else:
            with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as zf:
                for path in sorted(stage.rglob("*")):
                    if path.is_file():
                        relative = path.relative_to(stage)
                        info = zipfile.ZipInfo.from_file(path, relative.as_posix())
                        info.create_system = 3
                        mode = 0o755 if path.name in ("eagle-desktop", "eagle-cli") else 0o644
                        info.external_attr = (stat.S_IFREG | mode) << 16
                        info.compress_type = zipfile.ZIP_DEFLATED
                        with path.open("rb") as source, zf.open(info, "w") as dest:
                            shutil.copyfileobj(source, dest)
    return archive


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True, choices=TARGETS)
    parser.add_argument("--version", required=True)
    parser.add_argument("--bin-dir", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    try:
        print(package(args.target, args.version, args.bin_dir, args.output))
    except (ValueError, OSError) as error:
        parser.exit(1, f"error: {error}\n")


if __name__ == "__main__":
    main()
