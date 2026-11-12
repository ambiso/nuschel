#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""TEST ONLY: inspect the exact ad-hoc format emitted by the pinned signer.

Independent bounded integrity evidence, not Apple's codesign/Gatekeeper verifier
or publisher identity authentication. Unknown formats fail closed. Format sources:
https://github.com/apple-oss-distributions/xnu/blob/main/EXTERNAL_HEADERS/mach-o/fat.h
https://github.com/apple-oss-distributions/xnu/blob/main/EXTERNAL_HEADERS/mach-o/loader.h
https://github.com/apple-oss-distributions/xnu/blob/main/osfmk/kern/cs_blobs.h
https://github.com/apple-oss-distributions/Security/blob/main/OSX/libsecurity_codesigning/lib/codedirectory.h
Pinned rcodesign0.29: verify.rs unconditionally parses its own empty CMS wrapper;
embedded_signature_builder.rs::create_empty_cms_signature emits that wrapper.
No Apple implementation code is copied here; this inspects our produced bytes.
"""
import hashlib
import struct


def require(condition, description):
    if not condition:
        raise ValueError(description)


def verify_adhoc(data, identifier, *, info=None, resources=None):
    require(48 <= len(data) <= 512 * 1024**2, "binary size")
    data = memoryview(data)
    require(bytes(data[:4]) == b"\xca\xfe\xba\xbe", "unsupported fat format")
    require(struct.unpack_from(">I", data, 4)[0] == 2, "expected exactly two architectures")
    results, ranges, cpus = [], [], set()
    for index in range(2):
        cpu, subtype, offset, size, align = struct.unpack_from(">IIIII", data, 8 + 20 * index)
        require(cpu in (0x1000007, 0x100000c) and cpu not in cpus, "foreign/duplicate architecture")
        cpus.add(cpu)
        require(0 <= align <= 24 and offset >= 48 and offset % (1 << align) == 0,
                "fat alignment/offset")
        require(size >= 32 and offset + size <= len(data), "fat slice outside file")
        require(all(offset + size <= start or offset >= end for start, end in ranges), "overlapping slices")
        ranges.append((offset, offset + size))
        thin = data[offset:offset + size]
        magic, mcpu, msub, kind, count, command_bytes, _, reserved = struct.unpack_from("<8I", thin)
        require((magic, mcpu, msub, kind, reserved) == (0xfeedfacf, cpu, subtype, 2, 0),
                "unsupported thin executable")
        require(1 <= count <= 2048 and 8 <= command_bytes <= 1024**2 and
                32 + command_bytes <= size, "load-command bounds")
        position, signatures, segments = 32, [], []
        for _ in range(count):
            require(position + 8 <= 32 + command_bytes, "truncated load command")
            cmd, length = struct.unpack_from("<II", thin, position)
            require(length >= 8 and length % 8 == 0 and position + length <= 32 + command_bytes,
                    "load-command length")
            if cmd == 0x1d:
                require(length == 16, "signature command size")
                signatures.append(struct.unpack_from("<II", thin, position + 8))
            if cmd == 0x19:
                require(length >= 72, "segment command size")
                name = bytes(thin[position + 8:position + 24]).rstrip(b"\0")
                file_offset, file_size = struct.unpack_from("<QQ", thin, position + 40)
                require(file_offset + file_size <= size, "segment outside slice")
                segments.append((name, file_offset, file_size))
            position += length
        require(position == 32 + command_bytes and len(signatures) == 1, "signature command cardinality")
        sig_offset, sig_size = signatures[0]
        require(sig_offset >= position and 12 <= sig_size <= 8 * 1024**2 and sig_offset + sig_size == size,
                "signature bounds/end")
        require(segments and segments[-1][0] == b"__LINKEDIT" and
                segments[-1][1] <= sig_offset and sum(segments[-1][1:]) == size,
                "signature must end last LINKEDIT segment")
        signature = thin[sig_offset:]
        magic, length, entries = struct.unpack_from(">III", signature)
        require(magic == 0xfade0cc0 and entries == 3 and 36 <= length <= sig_size, "unsupported SuperBlob")
        require(not any(signature[length:]), "nonzero signature padding")
        blobs, blob_ranges = {}, []
        for item in range(entries):
            slot, start = struct.unpack_from(">II", signature, 12 + item * 8)
            require(slot in (0, 2, 0x10000) and slot not in blobs and 36 <= start <= length - 8,
                    "unknown/duplicate/out-of-bounds signature slot")
            blob_magic, blob_length = struct.unpack_from(">II", signature, start)
            require(8 <= blob_length <= length - start, "blob bounds")
            end = start + blob_length
            require(all(end <= a or start >= b for a, b in blob_ranges), "overlapping blobs")
            blob_ranges.append((start, end))
            blobs[slot] = (blob_magic, signature[start:end], start)
        require(set(blobs) == {0, 2, 0x10000}, "missing signature components")
        require(bytes(blobs[0x10000][1]) == struct.pack(">II", 0xfade0b01, 8), "CMS identity not permitted")
        require(bytes(blobs[2][1]) == struct.pack(">III", 0xfade0c01, 12, 0), "unsupported requirements")
        cd = blobs[0][1]
        require(len(cd) >= 88 and blobs[0][0] == 0xfade0c02, "CodeDirectory header")
        _, cd_length, version, flags, hashes, ident, specials, pages, limit, hs, ht, platform, page_log, spare = struct.unpack_from(">9I4BI", cd)
        require(cd_length == len(cd) and version == 0x20400 and flags == 2, "unsupported CodeDirectory/ADHOC flags")
        require((hs, ht, platform, page_log, spare) == (32, 2, 0, 12, 0), "unsupported hash/page format")
        require(struct.unpack_from(">IIIQ", cd, 44) == (0, 0, 0, 0), "scatter/team/extended-limit unsupported")
        exec_start, exec_limit, exec_flags = struct.unpack_from(">QQQ", cd, 64)
        require(exec_start + exec_limit <= limit and exec_flags == 1, "unsupported executable segment")
        expected_specials = 3 if info is not None or resources is not None else 2
        require(specials == expected_specials and 88 <= ident < hashes - 32 * specials,
                "special-slot/identifier bounds")
        name_region = bytes(cd[ident:hashes - 32 * specials])
        require(name_region == identifier.encode("ascii") + b"\0", "CodeDirectory identifier")
        require(limit == sig_offset and pages == (limit + 4095) // 4096 and
                hashes + 32 * pages == len(cd), "code hash coverage")
        for page in range(pages):
            digest = hashlib.sha256(thin[page * 4096:min((page + 1) * 4096, limit)]).digest()
            require(digest == cd[hashes + page * 32:hashes + (page + 1) * 32], "code-page digest mismatch")
        expected = {1: info, 2: blobs[2][1], 3: resources}
        for slot in range(1, specials + 1):
            value = expected[slot]
            digest = hashlib.sha256(value).digest() if value is not None else bytes(32)
            require(digest == cd[hashes - slot * 32:hashes - (slot - 1) * 32], "special-slot digest mismatch")
        results.append({"cpu": hex(cpu), "identifier": identifier, "code_pages": pages,
                        "code_directory_sha256": hashlib.sha256(cd).hexdigest(),
                        "code_limit": limit, "special_slots": specials,
                        "slice_offset": offset, "signature_offset": sig_offset,
                        "directory_offset": blobs[0][2], "hashes_offset": hashes,
                        "ad_hoc_no_cms_identity": True})
    last = 48
    for start, end in sorted(ranges):
        require(not any(data[last:start]), "nonzero fat padding")
        last = end
    require(last == len(data), "unexpected fat trailer")
    return results
