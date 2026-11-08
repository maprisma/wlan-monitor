"""Check installer Exec arguments through both desktop escaping layers."""
import pathlib
import shlex
import subprocess
import unittest


class DesktopExecTests(unittest.TestCase):
    def test_full_quoted_path_round_trip(self):
        installer = (pathlib.Path(__file__).resolve().parents[1] / "install.sh").read_text()
        function = installer.split("desktop_arg() {", 1)[1].split("\n}", 1)[0]
        script = "desktop_arg() {" + function + '\n}\ndesktop_arg "$1"'
        for path in ["/home/user/.local/bin/app", '/tmp/a b/quote"slash\\tick`dollar$percent%/app%']:
            with self.subTest(path=path):
                encoded = subprocess.check_output(["bash", "-c", script, "test", path], text=True)
                self.assertTrue(encoded.startswith('"') and encoded.endswith('"'))
                # Desktop value unescaping, followed by Exec argument parsing.
                value = encoded.replace("\\\\", "\\")
                argv = shlex.split(value)
                self.assertEqual(len(argv), 1)
                self.assertEqual(argv[0].replace("\\$", "$").replace("\\`", "`").replace("%%", "%"), path)
