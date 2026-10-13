#!/usr/bin/env python3
"""Check the format reference; --write regenerates the encoding fixtures in testdata/."""

import argparse
import hashlib
import json
import struct
from pathlib import Path

DATA = Path(__file__).resolve().parents[1] / 'testdata'
UNICODE = DATA / 'unicode' / '16.0.0'
UUID = bytes.fromhex('00112233445566778899aabbccddeeff')


def check(condition, message):
    if not condition:
        raise ValueError(message)


def crc32c(data):
    crc = 0xFFFFFFFF
    for byte in data:
        crc ^= byte
        for _ in range(8):
            crc = (crc >> 1) ^ (0x82F63B78 if crc & 1 else 0)
    return crc ^ 0xFFFFFFFF


class Unicode16:
    def __init__(self):
        self.category, self.ccc, self.decomposition, self.fold = {}, {}, {}, {}
        start = None
        for line in (UNICODE / 'UnicodeData.txt').read_text(encoding='utf-8').splitlines():
            fields = line.split(';')
            cp = int(fields[0], 16)
            if fields[1].endswith(', First>'):
                start = cp
                continue
            if fields[1].endswith(', Last>'):
                check(start is not None, 'Unicode range lacks start')
                for member in range(start, cp + 1):
                    self.category[member] = fields[2]
                start = None
                continue
            self.category[cp] = fields[2]
            if int(fields[3]):
                self.ccc[cp] = int(fields[3])
            if fields[5] and not fields[5].startswith('<'):
                self.decomposition[cp] = tuple(int(x, 16) for x in fields[5].split())
        check(start is None, 'Unicode range lacks end')
        excluded = set()
        for line in (UNICODE / 'CompositionExclusions.txt').read_text(encoding='utf-8').splitlines():
            text = line.split('#')[0].strip()
            if text:
                excluded.add(int(text, 16))
        self.composition = {pair: cp for cp, pair in self.decomposition.items()
                            if len(pair) == 2 and cp not in excluded
                            and self.ccc.get(pair[0], 0) == 0}
        for line in (UNICODE / 'CaseFolding.txt').read_text(encoding='utf-8').splitlines():
            text = line.split('#')[0].strip()
            if text:
                cp, status, mapping, _ = (x.strip() for x in text.split(';'))
                if status in ('C', 'F'):
                    self.fold[int(cp, 16)] = tuple(int(x, 16) for x in mapping.split())

    def decompose(self, cp):
        if 0xAC00 <= cp < 0xAC00 + 11172:
            index = cp - 0xAC00
            result = [0x1100 + index // 588, 0x1161 + (index % 588) // 28]
            if index % 28:
                result.append(0x11A7 + index % 28)
            return result
        if cp in self.decomposition:
            return [child for part in self.decomposition[cp] for child in self.decompose(part)]
        return [cp]

    def compose(self, first, second):
        if 0x1100 <= first < 0x1113 and 0x1161 <= second < 0x1176:
            return 0xAC00 + ((first - 0x1100) * 21 + second - 0x1161) * 28
        if (0xAC00 <= first < 0xD7A4 and (first - 0xAC00) % 28 == 0
                and 0x11A8 <= second < 0x11C3):
            return first + second - 0x11A7
        return self.composition.get((first, second))

    def nfc(self, text):
        ordered = []
        for cp in (part for char in text for part in self.decompose(ord(char))):
            ordered.append(cp)
            cc = self.ccc.get(cp, 0)
            index = len(ordered) - 1
            # Canonical reordering must preserve the order of equal-class marks.
            while cc and index and self.ccc.get(ordered[index - 1], 0) > cc:
                ordered[index], ordered[index - 1] = ordered[index - 1], ordered[index]
                index -= 1
        result, starter, last_cc = [], None, 0
        for cp in ordered:
            cc = self.ccc.get(cp, 0)
            combined = self.compose(result[starter], cp) if starter is not None else None
            if combined is not None and (last_cc == 0 or last_cc < cc):
                result[starter] = combined
            else:
                if cc == 0:
                    starter = len(result)
                result.append(cp)
                last_cc = cc
        return ''.join(chr(cp) for cp in result)

    def name(self, text):
        check(len(text.encode('utf-8')) <= 1024, 'raw name too long')
        for char in text:
            check(self.category.get(ord(char), 'Cn') not in ('Cn', 'Cs', 'Cc'),
                  'unassigned, surrogate, or control character')
        stored = self.nfc(text)
        check(stored and stored not in ('.', '..'), 'empty or dot name')
        check('/' not in stored, 'path separator')
        check(len(stored.encode('utf-8')) <= 255, 'stored name too long')
        folded = self.nfc(''.join(chr(cp) for char in stored
                                  for cp in self.fold.get(ord(char), (ord(char),))))
        check(len(folded.encode('utf-8')) <= 765, 'folded name too long')
        return stored, folded


def verify_unicode():
    manifest = json.loads((UNICODE / 'manifest.json').read_text(encoding='utf-8'))
    check(manifest['version'] == '16.0.0', 'wrong Unicode manifest version')
    sources = {'UnicodeData.txt', 'CompositionExclusions.txt', 'CaseFolding.txt',
               'NormalizationTest.txt', 'LICENSE.txt'}
    check(len(manifest['files']) == len(sources)
          and {item['name'] for item in manifest['files']} == sources,
          'Unicode manifest must pin each required source exactly once')
    for item in manifest['files']:
        data = (UNICODE / item['name']).read_bytes()
        check(len(data) == item['bytes'] and hashlib.sha256(data).hexdigest() == item['sha256'],
              'Unicode source changed: ' + item['name'])
    ucd = Unicode16()
    cases = 0
    for line in (UNICODE / 'NormalizationTest.txt').read_text(encoding='utf-8').splitlines():
        text = line.split('#')[0].strip()
        if not text or text.startswith('@'):
            continue
        columns = [''.join(chr(int(cp, 16)) for cp in field.split())
                   for field in text.split(';')[:5]]
        for index, value in enumerate(columns):
            check(ucd.nfc(value) == columns[1 if index < 3 else 3],
                  'NFC conformance failure: ' + text)
        cases += 1
    vectors = json.loads((DATA / 'name-vectors.json').read_text(encoding='utf-8'))
    check(vectors['unicode_version'] == '16.0.0' and vectors['profile'] == 2,
          'wrong filename vector profile')
    check(len(vectors['names']) == 35 and len({case['input'] for case in vectors['names']}) == 35,
          'filename corpus must contain the 35 distinct reviewed inputs')
    check(len(vectors['comparisons']) == 12
          and len({(case['left'], case['right']) for case in vectors['comparisons']}) == 12,
          'comparison corpus must contain the 12 distinct reviewed pairs')
    for case in vectors['names']:
        try:
            stored, folded = ucd.name(case['input'])
        except (ValueError, UnicodeError):
            check(not case['accepted'], 'unexpected name rejection: ' + repr(case['input']))
        else:
            check(case['accepted'] and stored == case['stored'] and folded == case['key'],
                  'wrong name result: ' + repr(case['input']))
    for case in vectors['comparisons']:
        equal = ucd.name(case['left'])[1] == ucd.name(case['right'])[1]
        check(equal == case['equal'], 'wrong comparison: ' + str(case))
    for text, accepted in [('a' * 255, True), ('a' * 256, False), ('é' * 127, True),
                           ('é' * 128, False), ('e\u0301' * 127, True)]:
        try:
            ucd.name(text)
        except ValueError:
            check(not accepted, 'unexpected length rejection')
        else:
            check(accepted, 'accepted oversized name')
    print(f'PASS: {cases} Unicode NFC rows, {len(vectors["names"])} names, '
          f'{len(vectors["comparisons"])} comparisons, 5 length boundaries')


def fixtures():
    result = {}
    for label, block in [('root-a', 256), ('root-b', 65536 - 257)]:
        page = bytearray(4096)
        page[:8] = b'CFSXROOT'
        struct.pack_into('<IIII', page, 8, 1, 4096, 4096, 65536)
        page[24:40] = UUID
        struct.pack_into('<QQQ', page, 40, block, 7, 65536)
        struct.pack_into('<QQQQQQ', page, 64, 1024, 5, 1025, 7, 1026, 7)
        struct.pack_into('<QQQIIQQ', page, 112, 18, 50000, 0, 2, 0, 16384, 9)
        struct.pack_into('<QQ', page, 160, 32, 7)
        struct.pack_into('<I', page, 4092, crc32c(page))
        result[label] = (bytes(page), 4092)
    page = bytearray(result['root-a'][0])
    name = 'Andrei, Backup \\ 東京'.encode('utf-8')
    struct.pack_into('<H', page, 184, len(name))
    page[186:186 + len(name)] = name
    page[4092:] = bytes(4)
    struct.pack_into('<I', page, 4092, crc32c(page))
    result['labeled-root-a'] = (bytes(page), 4092)
    page = bytearray(4096)
    page[:8] = b'CFSXPAGE'
    struct.pack_into('<HHBBBBQQHHI', page, 8, 1, 0, 1, 1, 0, 0, 1024, 5, 0, 0, 0)
    page[40:56] = UUID
    struct.pack_into('<I', page, 64, crc32c(page))
    result['empty-catalog-leaf'] = (bytes(page), 64)
    page[64:68] = bytes(4)
    key = struct.pack('>QBQ', 2, 3, 0)
    value = struct.pack('<QII', 2048, 16, 0)
    record = struct.pack('<HH', len(key), len(value)) + key + value
    struct.pack_into('<HH', page, 32, 1, len(record))
    struct.pack_into('<Q', page, 56, 16)
    page[80:80 + len(record)] = record
    struct.pack_into('<I', page, 64, crc32c(page))
    result['extent-catalog-leaf'] = (bytes(page), 64)
    # Object 2's 16-block extent and its compact checksum group: one present
    # slot, so the value ends after slot 0 at 36 bytes.
    group = bytearray(36)
    group[0] = 1
    struct.pack_into('<I', group, 32, 0x1234ABCD)
    records = b''.join(struct.pack('<HH', len(key), len(value)) + key + value
                       for key, value in [(struct.pack('>QBQ', 2, 3, 0), struct.pack('<QII', 2048, 16, 0)),
                                          (struct.pack('>QBQ', 2, 4, 0), bytes(group))])
    page = bytearray(4096)
    page[:8] = b'CFSXPAGE'
    struct.pack_into('<HHBBBBQQHHI', page, 8, 1, 0, 1, 1, 0, 0, 1024, 5, 2, len(records), 0)
    page[40:56] = UUID
    struct.pack_into('<Q', page, 56, 16)
    page[80:80 + len(records)] = records
    struct.pack_into('<I', page, 64, crc32c(page))
    result['checksum-catalog-leaf'] = (bytes(page), 64)
    # Root's entry "link" names object 2, a symlink whose target follows its
    # 128 fixed inode bytes; size is the target length.
    target = b'../notes.txt'
    name = b'link'
    entry = struct.pack('<QBBH', 2, 3, 0, len(name)) + name
    inode = bytearray(128)
    struct.pack_into('<BBxxxxxxQQQQ', inode, 0, 3, 0, 1, len(target), 0, 0)
    for offset in (40, 56, 72):
        struct.pack_into('<qI', inode, offset, 1_700_000_000, 0)
    records = b''.join(struct.pack('<HH', len(key), len(value)) + key + value
                       for key, value in [(struct.pack('>QB', 1, 2) + name, entry),
                                          (struct.pack('>QB', 2, 1), bytes(inode) + target)])
    page = bytearray(4096)
    page[:8] = b'CFSXPAGE'
    struct.pack_into('<HHBBBBQQHHI', page, 8, 1, 0, 1, 1, 0, 0, 1024, 5, 2, len(records), 0)
    page[40:56] = UUID
    page[80:80 + len(records)] = records
    struct.pack_into('<I', page, 64, crc32c(page))
    result['symlink-catalog-leaf'] = (bytes(page), 64)
    page = bytearray(4096)
    page[:8] = b'CFSXPAGE'
    # Workspace leaf records are runs: first block big-endian, slot count.
    records = b''.join(struct.pack('<HH', 8, 4) + struct.pack('>Q', first)
                       + struct.pack('<I', count)
                       for first, count in [(8192, 1), (9000, 2)])
    struct.pack_into('<HHBBBBQQHHI', page, 8, 1, 0, 1, 4, 0, 0, 32, 7,
                     2, len(records), 0)
    page[40:56] = UUID
    page[80:80 + len(records)] = records
    struct.pack_into('<I', page, 64, crc32c(page))
    result['workspace-leaf'] = (bytes(page), 64)
    return result


def verify_checksums(write):
    for data, expected in [(b'', 0), (b'123456789', 0xE3069283),
                           (bytes(32), 0x8A9136AA), (bytes([255]) * 32, 0x62A8AB43),
                           (bytes(range(32)), 0x46DD794E)]:
        check(crc32c(data) == expected, 'CRC32C reference failure')
    encoded = fixtures()
    vector_dir = DATA / 'vectors'
    if write:
        vector_dir.mkdir(exist_ok=True)
    records = []
    for name, (page, offset) in encoded.items():
        filename = name + '.bin'
        if write:
            (vector_dir / filename).write_bytes(page)
        check((vector_dir / filename).read_bytes() == page, 'encoding mismatch: ' + name)
        raw = bytearray(page)
        expected = struct.unpack_from('<I', raw, offset)[0]
        raw[offset:offset + 4] = bytes(4)
        check(crc32c(raw) == expected, 'stored CRC mismatch')
        for index in [0, 24, 40, 48, 56, 80, 136, 2048, 4091]:
            changed = bytearray(raw)
            changed[index] ^= 1
            check(crc32c(changed) != expected, 'CRC omitted a covered byte')
        records.append(dict(name=name, file='vectors/' + filename, crc_offset=offset,
                            crc32c=f'{expected:08x}', sha256=hashlib.sha256(page).hexdigest()))
    unit_records = []
    for object_id, unit, valid, data in [(2, 0, 5, b'hello'), (3, 0, 5, b'hello'),
                                        (2, 1, 5, b'hello'), (2, 0, 6, b'hello\0'),
                                        (2, 0, 65536, bytes(65536))]:
        prefix = b'CFSXDATA' + UUID + struct.pack('<QQI', object_id, unit, valid)
        crc = crc32c(prefix + data + bytes(65536 - len(data)))
        unit_records.append(dict(object_id=object_id, unit_index=unit, valid_bytes=valid,
                                 payload_hex=data.hex() if valid < 65536 else None,
                                 payload='zeros' if valid == 65536 else 'hex', crc32c=f'{crc:08x}'))
    check(len({case['crc32c'] for case in unit_records}) == len(unit_records),
          'identity/tail vectors accidentally coincide')
    document = dict(revision=1, root_revisions=[1], uuid_hex=UUID.hex(), fixtures=records, data_units=unit_records)
    path = DATA / 'checksum-vectors.json'
    if write:
        path.write_text(json.dumps(document, indent=2) + '\n', encoding='utf-8')
    check(json.loads(path.read_text(encoding='utf-8')) == document, 'checksum vectors changed')
    a, b = (bytearray(encoded[key][0]) for key in ('root-a', 'root-b'))
    for page in [a, b]:
        page[40:48] = bytes(8)
        page[4092:] = bytes(4)
    check(a == b, 'same-generation mirrors differ semantically')
    print(f'PASS: 5 standard CRCs, {len(encoded)} encoding fixtures, 5 data-identity/tail vectors')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--write', action='store_true', help='regenerate the declared encoding vectors')
    args = parser.parse_args()
    verify_unicode()
    verify_checksums(args.write)
