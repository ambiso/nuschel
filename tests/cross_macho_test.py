#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Tamper ACTUAL produced ad-hoc Mach-O bytes, not generated parser fixtures."""
import argparse
from pathlib import Path
import struct
import unittest
from cross_macho import verify_adhoc

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--app", type=Path, required=True)
arguments, remaining = parser.parse_known_args()
contents = arguments.app / "Contents"


class RealAdhoc(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.binary = (contents / "MacOS/nuschl").read_bytes()
        cls.info = (contents / "Info.plist").read_bytes()
        cls.resources = (contents / "_CodeSignature/CodeResources").read_bytes()
        cls.slices = cls.verify(cls.binary)

    @classmethod
    def verify(cls, binary, info=None, resources=None):
        return verify_adhoc(binary, "org.nuschl.Nuschl",
            info=cls.info if info is None else info,
            resources=cls.resources if resources is None else resources)

    def test_real_main_has_two_verified_page_sets_and_special_slots(self):
        self.assertEqual({s["cpu"] for s in self.slices}, {"0x1000007", "0x100000c"})
        self.assertTrue(all(s["code_pages"] > 100 and s["special_slots"] == 3 for s in self.slices))

    def test_real_helpers_are_separately_verified(self):
        for algorithm in ("ed25519", "mldsa44"):
            name = "nuschl-update-verify-" + algorithm
            self.assertEqual(len(verify_adhoc((contents / "MacOS" / name).read_bytes(), name)), 2)

    def test_code_page_tampering_in_either_actual_architecture(self):
        for part in self.slices:
            with self.subTest(cpu=part["cpu"]):
                changed = bytearray(self.binary)
                changed[part["slice_offset"] + 8192] ^= 1
                with self.assertRaisesRegex(ValueError, "code-page"):
                    self.verify(changed)

    def test_actual_info_plist_tampering_is_bound_to_main(self):
        with self.assertRaisesRegex(ValueError, "special-slot"):
            self.verify(self.binary, info=self.info + b"\n")

    def test_actual_resource_manifest_tampering_is_bound_to_main(self):
        with self.assertRaisesRegex(ValueError, "special-slot"):
            self.verify(self.binary, resources=self.resources + b"\n")

    def test_fat_table_outside_actual_file_is_rejected(self):
        changed = bytearray(self.binary)
        struct.pack_into(">I", changed, 16, len(changed) + 4096)
        with self.assertRaises(ValueError):
            self.verify(changed)

    def test_actual_load_command_table_corruption_is_rejected(self):
        changed = bytearray(self.binary)
        struct.pack_into("<I", changed, self.slices[0]["slice_offset"] + 36, 0xfffffff8)
        with self.assertRaisesRegex(ValueError, "load-command"):
            self.verify(changed)

    def test_actual_superblob_cannot_point_inside_its_own_table(self):
        changed = bytearray(self.binary)
        part = self.slices[0]
        base = part["slice_offset"] + part["signature_offset"]
        struct.pack_into(">I", changed, base + 16, 12)
        with self.assertRaisesRegex(ValueError, "signature slot"):
            self.verify(changed)

    def test_actual_code_hash_table_tampering_is_rejected(self):
        changed = bytearray(self.binary)
        part = self.slices[0]
        changed[part["slice_offset"] + part["signature_offset"] + part["directory_offset"] + part["hashes_offset"]] ^= 1
        with self.assertRaisesRegex(ValueError, "code-page"):
            self.verify(changed)

    def test_actual_directory_unknown_hash_format_is_rejected(self):
        changed = bytearray(self.binary)
        part = self.slices[0]
        changed[part["slice_offset"] + part["signature_offset"] + part["directory_offset"] + 37] = 1
        with self.assertRaisesRegex(ValueError, "hash/page"):
            self.verify(changed)

    def test_actual_empty_cms_wrapper_cannot_become_an_identity(self):
        changed = bytearray(self.binary)
        part = self.slices[0]
        base = part["slice_offset"] + part["signature_offset"]
        entries = [struct.unpack_from(">II", changed, base + 12 + i * 8) for i in range(3)]
        cms = next(offset for slot, offset in entries if slot == 0x10000)
        length = struct.unpack_from(">I", changed, base + 4)[0]
        self.assertEqual(cms + 8, length)
        struct.pack_into(">I", changed, base + 4, length + 1)
        struct.pack_into(">I", changed, base + cms + 4, 9)
        changed[base + cms + 8] = 0x30
        with self.assertRaisesRegex(ValueError, "CMS identity"):
            self.verify(changed)


if __name__ == "__main__":
    unittest.main(argv=[__file__, *remaining])
