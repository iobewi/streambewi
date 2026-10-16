#!/usr/bin/env python3
"""Package the product layout without espflash 4.6's custom-data-subtype panic.

espflash supplies the application image and its upstream S3 bootloader. Espressif's
pinned generator validates/encodes the table; esptool performs the final merge.
No partition encoder or ESP image encoder is implemented here.
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


def package(elf, output):
    gen = generator()
    table = gen.PartitionTable.from_csv((ROOT / 'targets/esp32/partitions.csv').read_text())
    table.verify()
    table.verify_size_fits(4 * 1024 * 1024)
    factory = next(p for p in table if p.name == 'factory' and p.type == gen.APP_TYPE)
    config = next(p for p in table if p.name == 'config')
    if config.type != gen.DATA_TYPE or config.subtype != 0x40:
        raise ValueError('expected config data/0x40 for EKV')
    with tempfile.TemporaryDirectory(prefix='streambewi-package-') as temp:
        work = Path(temp)
        app, default = work / 'app.bin', work / 'default.bin'
        base = ['espflash', 'save-image', '--chip', 'esp32s3', '--flash-size', '4mb',
                '--flash-mode', 'dio', '--flash-freq', '40mhz']
        subprocess.run([*base, str(elf), str(app)], check=True)
        subprocess.run([*base, '--merge', '--skip-padding', str(elf), str(default)], check=True)
        reference = default.read_bytes()
        if reference[:1] != b'\xe9' or reference[0x8000:0x8002] != b'\xaa\x50':
            raise ValueError('unexpected espflash upstream bootloader/table layout')
        if app.stat().st_size > factory.size:
            raise ValueError('application does not fit the factory partition')
        boot, partitions = work / 'boot.bin', work / 'partitions.bin'
        boot.write_bytes(reference[:0x8000])
        partitions.write_bytes(table.to_binary())
        output.parent.mkdir(parents=True, exist_ok=True)
        subprocess.run([sys.executable, '-m', 'esptool', '--chip', 'esp32s3', 'merge-bin',
                        '--flash-mode', 'dio', '--flash-freq', '40m', '--flash-size', '4MB',
                        '-o', str(output), '0x0', str(boot), '0x8000', str(partitions),
                        hex(factory.offset), str(app)], check=True)
        merged = output.read_bytes()
        if merged[0x8000:0x8000 + partitions.stat().st_size] != partitions.read_bytes():
            raise ValueError('merged partition table mismatch')
        if merged[factory.offset:factory.offset + app.stat().st_size] != app.read_bytes():
            raise ValueError('merged application mismatch')
        print(f'Product image: {output.stat().st_size} bytes; app at {factory.offset:#x}, '
              f'{app.stat().st_size}/{factory.size} bytes; config table subtype data/0x40 verified')


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('elf', type=Path)
    parser.add_argument('output', type=Path)
    args = parser.parse_args()
    package(args.elf.resolve(), args.output.resolve())
