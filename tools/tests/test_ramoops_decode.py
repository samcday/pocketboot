import gzip
import importlib.util
from pathlib import Path
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location('ramoops', ROOT / 'tools/ramoops_decode.py')
ramoops = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ramoops)


class RamoopsTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temp = tempfile.TemporaryDirectory()
        cls.codec = ramoops.Codec(ROOT / 'target/kernel/src/msm8916', cls.temp.name)
        fixture = Path(__file__).with_name('fixtures') / 'a5u-controlled-panic.bin.gz'
        cls.raw = gzip.decompress(fixture.read_bytes())

    @classmethod
    def tearDownClass(cls):
        cls.temp.cleanup()

    def test_real_kernel_panic_and_compression(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp)
            result = ramoops.decode_capture(self.raw, 64, self.codec, out)
            self.assertIn(b'Kernel panic - not syncing: sysrq triggered crash',
                          (out / 'dmesg-0.log').read_bytes())
            self.assertIn(b'Rebooting in 5 seconds', (out / 'console.log').read_bytes())
            self.assertFalse(any('error' in zone for zone in result['regions'].values()))

    def test_real_kernel_ecc_recovers_header_data_and_parity(self):
        zone = self.raw[:0x2000]
        expected, _ = ramoops.decode_zone(zone, 64, self.codec)
        damaged = bytearray(zone)
        for offset in [0, 4, 8, 12, 18, 35, 0x1fff]:
            damaged[offset] ^= 0xa5
        actual, info = ramoops.decode_zone(damaged, 64, self.codec)
        self.assertEqual(actual, expected)
        self.assertEqual(info['corrected_symbols'], 7)
        self.assertEqual(info['uncorrectable_blocks'], [])

    def test_uncorrectable_block_is_exposed(self):
        damaged = bytearray(self.raw[:0x2000])
        for offset in range(12, 77):
            damaged[offset] ^= 0xa5
        _, info = ramoops.decode_zone(damaged, 64, self.codec)
        self.assertIn(0, info['uncorrectable_blocks'])

    def test_truncated_capture_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaises(ValueError):
                ramoops.decode_capture(self.raw[:-1], 64, self.codec, Path(tmp))


if __name__ == '__main__':
    unittest.main()
