import hashlib
import json
import tempfile
import unittest
import zipfile
from pathlib import Path

from verify_release import ASSETS, verify_dist, verify_metadata


class ReleaseVerificationTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "apps/tray").mkdir(parents=True)
        (self.root / "apps/tray/Cargo.toml").write_text(
            '[package]\nversion = "1.2.3"\n', encoding="utf-8"
        )
        self.channel = {"history": [{"version": "1.2.3", "title": "更新", "body": "修复问题"}]}
        (self.root / "app_update.json").write_text(
            json.dumps(self.channel, ensure_ascii=False), encoding="utf-8"
        )

    def make_dist(self):
        dist = self.root / "dist"
        dist.mkdir()
        (dist / "voicetype-tray.exe").write_bytes(b"compiled executable")
        (dist / "voicetype-setup-x64.exe").write_bytes(b"installer")
        with zipfile.ZipFile(dist / ASSETS[0], "w") as archive:
            archive.writestr("voicetype-tray.exe", b"compiled executable")
            archive.writestr("README.md", b"readme")
            archive.write(self.root / "app_update.json", "app_update.json")
        for name in ASSETS:
            digest = hashlib.sha256((dist / name).read_bytes()).hexdigest()
            (dist / f"{name}.sha256").write_text(f"{digest}  {name}\n", encoding="ascii")
        return dist

    def test_metadata_requires_same_version_in_code_channel_and_tag(self):
        self.assertEqual(verify_metadata(self.root, "v1.2.3"), "1.2.3")
        with self.assertRaisesRegex(ValueError, "版本不一致"):
            verify_metadata(self.root, "v1.2.4")
        self.channel["history"][0]["version"] = "1.2.4"
        (self.root / "app_update.json").write_text(
            json.dumps(self.channel, ensure_ascii=False), encoding="utf-8"
        )
        with self.assertRaisesRegex(ValueError, "版本不一致"):
            verify_metadata(self.root, None)

    def test_accepts_consistent_release_assets(self):
        verify_dist(self.root, self.make_dist())

    def test_rejects_modified_asset(self):
        dist = self.make_dist()
        (dist / "voicetype-tray.exe").write_bytes(b"modified executable")
        with self.assertRaisesRegex(ValueError, "SHA-256 不匹配"):
            verify_dist(self.root, dist)


if __name__ == "__main__":
    unittest.main()
