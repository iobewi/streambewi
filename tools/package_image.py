#!/usr/bin/env python3
"""Package the product for the OTA layout of system A (#3), without a partition or ESP image encoder of ours.

Inputs: the firmware ELF, the second stage and the initial `otadata` (scripts/build-bootloader.sh).
espflash supplies the application image; Espressif's pinned generator validates and encodes the table; the
library's own validator (`iobewi-esp-boot` example `validate_image`) checks the application against both
slots; esptool performs the final merge.

Outputs next to OUTPUT:
  OUTPUT                      the whole flash up to 4 MiB: second stage, table, otadata, application in ota_0 and a
                              BLANK `config`. It is the explicit first install of a board: it erases the configuration.
  <name>-ota.bin              the application alone: what the OTA uploads into the inactive slot (`client.py`).
  <name>-ota.sha256           its digest, as the request header wants it: sha256:<hex>.
"""
import argparse
import hashlib
import importlib.util
from pathlib import Path
import subprocess
import sys
import tempfile
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
GEN_URL = 'https://raw.githubusercontent.com/espressif/esp-idf/v6.0/components/partition_table/gen_esp32part.py'
GEN_SHA256 = '02f6c1c5f012393d47987ee64bc50ba319a2eda48cbcec754d8cfaf780cce529'
FLASH = 4 * 1024 * 1024
VALIDATOR = ROOT / 'target/tools/validate_image'


def generator():
    path = ROOT / 'target/tools/gen_esp32part-v6.0.py'
    if not path.exists():
        with urllib.request.urlopen(GEN_URL, timeout=30) as response:
            source = response.read()
        if hashlib.sha256(source).hexdigest() != GEN_SHA256:
            raise ValueError('unexpected upstream partition generator checksum')
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(source)
    if hashlib.sha256(path.read_bytes()).hexdigest() != GEN_SHA256:
        raise ValueError('cached partition generator checksum mismatch')
    spec = importlib.util.spec_from_file_location('streambewi_idf_part', path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    module.offset_part_table = 0x8000
    return module


def layout(gen):
    """The table, checked against what the firmware and the library expect of the OTA layout."""
    table = gen.PartitionTable.from_csv((ROOT / 'targets/esp32/partitions.csv').read_text())
    table.verify()
    table.verify_size_fits(FLASH)
    by_name = {p.name: p for p in table}
    if set(by_name) != {'otadata', 'ota_0', 'ota_1', 'config'}:
        raise ValueError(f'unexpected partitions: {sorted(by_name)}')
    ota0, ota1, otadata, config = (by_name[n] for n in ('ota_0', 'ota_1', 'otadata', 'config'))
    if (ota0.type, ota0.subtype) != (gen.APP_TYPE, 0x10) or (ota1.type, ota1.subtype) != (gen.APP_TYPE, 0x11):
        raise ValueError('ota_0/ota_1 must be app/ota_0 and app/ota_1')
    if ota0.size != ota1.size:
        raise ValueError('the two slots must have the same size')
    if ota0.offset % 0x10000 or ota1.offset % 0x10000 or ota0.size % 0x1000:
        raise ValueError('application slots are 64 KiB aligned and a multiple of the 4 KiB erase size')
    if (otadata.type, otadata.subtype) != (gen.DATA_TYPE, 0) or otadata.size < 0x2000:
        raise ValueError('otadata must be data/ota, two 4 KiB sectors at least')
    if config.type != gen.DATA_TYPE or config.subtype != 0x40:
        raise ValueError('expected config data/0x40 for EKV')
    if not 32 <= config.size // 0x1000 <= 256:
        raise ValueError('config must have 32 to 256 EKV pages')
    return table, ota0, ota1, otadata, config


def package(elf, output, bootloader, otadata_image):
    gen = generator()
    table, ota0, ota1, otadata, config = layout(gen)
    boot = bootloader.read_bytes()
    if not 0 < len(boot) <= 0x8000:
        raise ValueError('the second stage must end before the table at 0x8000')
    if boot[2] != 0x02:
        raise ValueError('the second stage image header is not DIO')
    meta = otadata_image.read_bytes()
    if len(meta) != 0x2000:
        raise ValueError('the initial otadata must be 2 x 4 KiB')
    with tempfile.TemporaryDirectory(prefix='streambewi-package-') as temp:
        work = Path(temp)
        app = work / 'app.bin'
        subprocess.run(['espflash', 'save-image', '--chip', 'esp32s3', '--flash-size', '4mb',
                        '--flash-mode', 'dio', '--flash-freq', '40mhz', str(elf), str(app)], check=True)
        image = app.read_bytes()
        if image[:1] != b'\xe9' or image[2] != 0x02:
            raise ValueError('the application is not a DIO ESP image')
        if len(image) > ota0.size:
            raise ValueError(f'application ({len(image)} bytes) does not fit a slot ({ota0.size})')
        # The library's validator, the one the boot and the OTA use, against each slot.
        if not VALIDATOR.exists():
            raise FileNotFoundError(f'{VALIDATOR}: run scripts/build-bootloader.sh first')
        for slot in (ota0, ota1):
            subprocess.run([str(VALIDATOR), str(app), f'{slot.offset:x}', f'{slot.size:x}'], check=True)
        partitions = work / 'partitions.bin'
        partitions.write_bytes(table.to_binary())
        output.parent.mkdir(parents=True, exist_ok=True)
        subprocess.run([sys.executable, '-m', 'esptool', '--chip', 'esp32s3', 'merge-bin',
                        '--flash-mode', 'dio', '--flash-freq', '40m', '--flash-size', '4MB',
                        '--pad-to-size', '4MB', '-o', str(output),
                        '0x0', str(bootloader), '0x8000', str(partitions),
                        hex(otadata.offset), str(otadata_image), hex(ota0.offset), str(app)], check=True)
        merged = output.read_bytes()
        if len(merged) != FLASH:
            raise ValueError(f'merged image is {len(merged)} bytes, expected {FLASH}')
        for what, offset, data in (('second stage', 0, boot), ('partition table', 0x8000, partitions.read_bytes()),
                                   ('otadata', otadata.offset, meta), ('application', ota0.offset, image)):
            if merged[offset:offset + len(data)] != data:
                raise ValueError(f'merged {what} mismatch')
        if merged[ota1.offset:ota1.offset + ota1.size] != b'\xff' * ota1.size:
            raise ValueError('ota_1 must be blank in the first install image')
        if merged[config.offset:config.offset + config.size] != b'\xff' * config.size:
            raise ValueError('config must be blank in the first install image')
        stem = output.with_suffix('')
        ota = stem.with_name(stem.name + '-ota.bin')
        ota.write_bytes(image)
        digest = hashlib.sha256(image).hexdigest()
        stem.with_name(stem.name + '-ota.sha256').write_text(f'sha256:{digest}\n')
        print(f'First install image: {output.stat().st_size} bytes (second stage {len(boot)}, otadata, app at '
              f'{ota0.offset:#x}, config blank at {config.offset:#x}); OTA payload: {len(image)}/{ota0.size} bytes, '
              f'sha256:{digest}')


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('elf', type=Path, nargs='?')
    parser.add_argument('output', type=Path, nargs='?')
    parser.add_argument('--bootloader', type=Path)
    parser.add_argument('--otadata', type=Path)
    parser.add_argument('--check-layout', action='store_true',
                        help='only check targets/esp32/partitions.csv (no build, no device)')
    args = parser.parse_args()
    if args.check_layout:
        table, ota0, ota1, otadata, config = layout(generator())
        print(f'Layout OK: otadata {otadata.offset:#x}+{otadata.size:#x}, ota_0 {ota0.offset:#x}+{ota0.size:#x}, '
              f'ota_1 {ota1.offset:#x}+{ota1.size:#x}, config {config.offset:#x}+{config.size:#x} '
              f'({config.size // 0x1000} EKV pages)')
    else:
        if not (args.elf and args.output and args.bootloader and args.otadata):
            parser.error('elf, output, --bootloader and --otadata are required')
        package(args.elf.resolve(), args.output.resolve(), args.bootloader.resolve(), args.otadata.resolve())
