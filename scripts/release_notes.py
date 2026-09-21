#!/usr/bin/env python3
"""从 app_update.json 提取指定版本的更新公告，生成 GitHub Release 正文。

CI 用法：python scripts/release_notes.py v0.2.0 dist/release_body.md
本地查看：python scripts/release_notes.py v0.2.0
"""
import json
import sys
from pathlib import Path

# Windows 控制台默认 cp1252/GBK，直接 print 中文会 UnicodeEncodeError。
if hasattr(sys.stdout, "reconfigure"):
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    sys.stderr.reconfigure(encoding="utf-8", errors="replace")

CHANNEL = Path(__file__).resolve().parent.parent / "app_update.json"


def main() -> int:
    if len(sys.argv) < 2:
        print("用法: release_notes.py <tag> [输出文件]", file=sys.stderr)
        return 2
    tag = sys.argv[1].lstrip("v")
    entries = json.loads(CHANNEL.read_text(encoding="utf-8-sig"))["history"]
    entry = next((e for e in entries if str(e.get("version", "")).lstrip("v") == tag), None)
    if entry is None:
        print(f"app_update.json 里没有 v{tag} 的公告条目", file=sys.stderr)
        return 1

    body = entry.get("body", "").strip()
    lines = [f"# VoiceType v{tag} 更新公告", ""]
    if entry.get("title"):
        lines += [f"**{entry['title'].strip()}**", ""]
    lines += [body, "", "---", ""]
    lines += [
        "| 形态 | 资产 | 说明 |",
        "| --- | --- | --- |",
        "| 便携版 | `voicetype-win-x64-portable.zip` | 解压即用，先运行 `setup-models.ps1` 下载模型 |",
        "| 安装版 | `voicetype-setup-x64.exe` | 向导安装，可选开机自启 |",
        "| 热更新 | `voicetype-tray.exe`（+ `.sha256`） | 已安装用户经托盘「检查更新」自动下载校验 |",
        "",
        "所有资产均附带 SHA-256 清单；应用内热更新会自动校验。",
    ]
    text = "\n".join(lines)

    if len(sys.argv) >= 3:
        out = Path(sys.argv[2])
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_text(text, encoding="utf-8")
        print(f"Release 公告已生成: {out}")
    else:
        print(text)
    return 0


if __name__ == "__main__":
    sys.exit(main())
