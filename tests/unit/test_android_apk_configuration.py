"""The publish gate checks the APK, not the builder's local configuration."""

import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location(
    "verify_android_apk", Path(__file__).resolve().parents[2] / "scripts/verify_android_apk.py",
)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class ApkConfigurationTest(unittest.TestCase):
    def dex(self, overrides=None):
        fields = dict.fromkeys(module.REQUIRED_FIELDS, "configured")
        fields.update(overrides or {})
        return "\n".join(f'.field public static final {key}:Ljava/lang/String; = "{value}"'
                         for key, value in fields.items())

    def test_accepts_complete_configuration(self):
        self.assertEqual([], module.missing_configuration(self.dex()))

    def test_rejects_missing_firebase_and_does_not_report_values(self):
        self.assertEqual(["SM_FIREBASE_API_KEY"], module.missing_configuration(
            self.dex({"SM_FIREBASE_API_KEY": ""}),
        ))

    def test_rejects_unknown_or_empty_dump(self):
        self.assertEqual(list(module.REQUIRED_FIELDS), module.missing_configuration(""))
        self.assertEqual(list(module.REQUIRED_FIELDS), module.missing_configuration("unexpected format"))

    def test_rejects_whitespace_only_value(self):
        self.assertEqual(["SM_FIREBASE_APP_ID"], module.missing_configuration(
            self.dex({"SM_FIREBASE_APP_ID": "  "}),
        ))


if __name__ == "__main__":
    unittest.main()
