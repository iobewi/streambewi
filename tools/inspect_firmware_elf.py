#!/usr/bin/env python3
"""Assert product descriptor and report native task storage / linker stack (ELF32 LE)."""
import json
from pathlib import Path
import struct
import sys

raw = Path(sys.argv[1]).read_bytes()
assert raw[:6] == b'\x7fELF\x01\x01', 'expected ELF32 little endian'
header = struct.unpack_from('<16sHHIIIIIHHHHHH', raw)
section_offset, section_size, section_count = header[6], header[11], header[12]
sections = [struct.unpack_from('<10I', raw, section_offset + i * section_size)
            for i in range(section_count)]
symbols = {}
for section in sections:
    if section[1] != 2:  # SHT_SYMTAB
        continue
    strings = sections[section[6]]
    names = raw[strings[4]:strings[4] + strings[5]]
    for pos in range(section[4], section[4] + section[5], section[9]):
        name, address, size, info, other, index = struct.unpack_from('<IIIBBH', raw, pos)
        symbols[names[name:].split(b'\0', 1)[0].decode()] = (address, size, index)

def data(name):
    address, size, index = symbols[name]
    section = sections[index]
    offset = section[4] + address - section[3]
    return raw[offset:offset + size]

desc = data('esp_app_desc')
version = desc[16:48].split(b'\0', 1)[0].decode()
product = desc[48:80].split(b'\0', 1)[0].decode()
assert product == 'streambewi-esp32', product
assert version == '0.1.0', version
assert data('STREAMBEWI_CHIP') == b'ESP32-S3'
result = {'descriptor_product': product, 'descriptor_version': version,
          'chip': data('STREAMBEWI_CHIP').decode()}
# These are actual compiler-emitted TaskPool allocations, including executor
# metadata. They bound static task storage, not runtime stack high-water marks.
pools = {name: {'address': hex(address), 'bytes': size}
         for name, (address, size, _) in symbols.items()
         if 'product' in name and 'POOL' in name and size > 0}
assert pools, 'product TaskPool symbol missing'
result['product_task_pools'] = pools
for name in ['_stack_end', '_stack_start']:
    result[name] = hex(symbols[name][0])
result['linker_stack_reservation_bytes'] = symbols['_stack_start'][0] - symbols['_stack_end'][0]
assert result['linker_stack_reservation_bytes'] >= 16 * 1024, 'below product stack reservation budget'
result['hardware_stack_high_water_mark'] = 'NOT MEASURED'
print(json.dumps(result, indent=2))
