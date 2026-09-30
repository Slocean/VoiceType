#!/usr/bin/env python3
"""发布门禁：核对版本、资产、SHA-256 清单与便携包内容。"""

import argparse
import hashlib
import json
import re
import sys
import zipfile
from pathlib import Path

# Windows 控制台默认 cp1252/GBK，直接 print 中文会 UnicodeEncodeError。
if hasattr(sys.stdout, "reconfigure"):
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    sys.stderr.reconfigure(encoding="utf-8", errors="replace")

ROOT = Path(__file__).resolve().parent.parent
ASSETS = (
    "voicetype-win-x64-portable.zip",
    "voicetype-setup-x64.exe",
    "voicetype-tray.exe",
)
MANIFEST = re.compile(rb"([0-9a-f]{64})  ([^\r\n]+)\r?\n?\Z")


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def verify_metadata(root: Path, tag: str | None) -> str:
    cargo = (root / "apps/tray/Cargo.toml").read_text(encoding="utf-8")
    match = re.search(r'^version\s*=\s*"([^"]+)"', cargo, re.MULTILINE)
    if match is None:
        raise ValueError("apps/tray/Cargo.toml 缺少 version")
    version = match.group(1)
    channel = json.loads((root / "app_update.json").read_text(encoding="utf-8-sig"))
    history = channel.get("history")
    if not isinstance(history, list) or not history:
        raise ValueError("app_update.json 缺少更新公告")
    latest = history[0]
    if latest.get("version") != version:
        raise ValueError(f"版本不一致：Cargo.toml={version}，公告={latest.get('version')}")
    if not latest.get("title", "").strip() or not latest.get("body", "").strip():
        raise ValueError(f"v{version} 的更新公告标题或正文为空")
    if tag and tag != f"v{version}":
        raise ValueError(f"版本不一致：tag={tag}，Cargo.toml={version}")
    return version


def verify_dist(root: Path, dist: Path) -> None:
    for name in ASSETS:
        asset = dist / name
        checksum = dist / f"{name}.sha256"
        if not asset.is_file() or not checksum.is_file():
            raise ValueError(f"缺少发布资产或 SHA-256 清单：{name}")
        match = MANIFEST.fullmatch(checksum.read_bytes())
        if match is None or match.group(2) != name.encode("ascii"):
            raise ValueError(f"SHA-256 清单格式或文件名错误：{checksum.name}")
        actual = sha256_file(asset)
        expected = match.group(1).decode("ascii")
        if actual != expected:
            raise ValueError(f"SHA-256 不匹配：{name}")

    portable = dist / ASSETS[0]
    with zipfile.ZipFile(portable) as archive:
        names = archive.namelist()
        required = {"voicetype-tray.exe", "README.md", "app_update.json"}
        if set(names) != required or len(names) != len(required):
            raise ValueError(f"便携包内容异常：{names!r}")
        if archive.read("app_update.json") != (root / "app_update.json").read_bytes():
            raise ValueError("便携包中的更新通道不是当前提交版本")
        exe_hash = hashlib.sha256(archive.read("voicetype-tray.exe")).hexdigest()
        if exe_hash != sha256_file(dist / "voicetype-tray.exe"):
            raise ValueError("便携 zip 中的 exe 与热更新裸 exe 不一致")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT)
    parser.add_argument("--dist", type=Path, help="检查打包产物目录；省略时只检查版本")
    parser.add_argument("--tag", help="推送的 tag，格式为 vX.Y.Z")
    args = parser.parse_args()
    try:
        version = verify_metadata(args.root, args.tag)
        if args.dist:
            verify_dist(args.root, args.dist)
    except (OSError, ValueError, KeyError, zipfile.BadZipFile) as error:
        print(f"发布验收失败：{error}", file=sys.stderr)
        return 1
    print(f"发布验收通过：v{version}" + ("，3 个资产及清单齐全" if args.dist else ""))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
