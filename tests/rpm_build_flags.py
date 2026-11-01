"""Check release object output using the RPM build's actual compiler flags."""

import os
import shlex
import subprocess
import tempfile
import unittest
from pathlib import Path


class ReleaseFlags(unittest.TestCase):
    def test_release_objects_omit_unused_debug_info(self):
        with tempfile.TemporaryDirectory() as temporary:
            source = Path(temporary) / "probe.rs"
            object_file = source.with_suffix(".o")
            source.write_text("#[used] static PROBE: fn(u64) -> u64 = |x| x.wrapping_add(1);\n")
            flags = shlex.split(os.environ["RUSTFLAGS"])
            subprocess.run(
                ["rustc", "--crate-type=lib", "--emit=obj", *flags, str(source)],
                cwd=temporary, check=True,
            )
            sections = subprocess.check_output(["readelf", "-SW", str(object_file)], text=True)
            symbols = subprocess.check_output(["nm", str(object_file)], text=True)
            self.assertIn("PROBE", symbols)
            self.assertNotIn(".debug_info", sections)


if __name__ == "__main__":
    unittest.main()
