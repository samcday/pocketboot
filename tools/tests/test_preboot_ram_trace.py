import importlib.util
from pathlib import Path
import struct
import unittest

spec = importlib.util.spec_from_file_location("trace", Path(__file__).parents[1] / "preboot_ram_trace.py")
trace = importlib.util.module_from_spec(spec)
spec.loader.exec_module(trace)


def fixture():
    page = bytearray(4096)
    page[:8] = b"PBTRACE1"
    struct.pack_into("<9Q", page, 8, 5, 6, 0x80000100, 4, 0x30d00800, 0, 0, 0,
                     0x85944171f73967e8)  # Independent known FNV-1a vector: "foobar".
    page[128:134] = b"foobar"
    return page * 3


class TraceTests(unittest.TestCase):
    def test_intact(self):
        self.assertEqual(trace.decode(fixture())["text"], "foobar")

    def test_compact_page_prefixes(self):
        data = fixture()
        prefixes = b"".join(data[page * 4096:page * 4096 + 256] for page in range(3))
        self.assertEqual(trace.decode(prefixes)["text"], "foobar")
        too_short = b"".join(data[page * 4096:page * 4096 + 128] for page in range(3))
        with self.assertRaises(ValueError):
            trace.decode(too_short)

    def test_recovers_distinct_reset_bit_losses(self):
        data = fixture()
        for page, offset in enumerate([128, 129, 130]):
            data[page * 4096 + offset] &= 0xfe
            data[page * 4096 + offset] &= 0xfb
        result = trace.decode(data)
        self.assertEqual(result["text"], "foobar")
        self.assertIn("majority", result["checksum_valid_recoveries"])

    def test_common_corruption_rejected(self):
        data = fixture()
        for page in range(3):
            data[page * 4096 + 128] ^= 1
        with self.assertRaises(ValueError):
            trace.decode(data)

    def test_invalid_length_rejected(self):
        data = fixture()
        for page in range(3):
            struct.pack_into("<Q", data, page * 4096 + 16, 4096)
        with self.assertRaises(ValueError):
            trace.decode(data)

    def test_header_disagreement_exposed(self):
        data = fixture()
        struct.pack_into("<Q", data, 32, 0)
        result = trace.decode(data)
        self.assertEqual(result["header"]["current_el"], "0x4")
        self.assertEqual(result["header_copy_disagreement"]["current_el"], ["0x0", "0x4", "0x4"])


if __name__ == "__main__":
    unittest.main()
