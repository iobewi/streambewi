#!/usr/bin/env python3
"""Static stack depth of the firmware ELF (docs/diagnostic-logs.md).

    tools/stack_depth.py ELF ROOT_REGEX [FUNCTION_REGEX ...]
    tools/stack_depth.py target/xtensa-esp32s3-none-elf/release/streambewi-esp32 \
        '^streambewi_esp32::__product_task::__product_task_inner_function::\{closure#0\}$' 'setup::mutation'

The GNU objdump cannot decode LLVM-built Xtensa code (no `.xt.prop`), so the instructions are decoded here.

Instructions are decoded from the function symbols' bytes (2- or 3-byte, no inline data in LLVM output):
`entry a1, N` gives the frame, `call4/8/12`, `j` and `callx` through a literal give the edges. The depth of a
function is the sum of the frames on the deepest chain of known calls from the root. Indirect calls through
a register that is not loaded from a literal, and interrupts, are not followed: it is a lower bound."""
import bisect, re, struct, subprocess, sys, collections

elf, root_pat = sys.argv[1], sys.argv[2]
patterns = sys.argv[3:]
def toolchain_prefix():
    """`xtensa-esp32s3-elf-` binutils, from the PATH or from the `esp` toolchain's sysroot."""
    import glob, os, shutil
    if os.environ.get("XTENSA_BINUTILS"):
        return os.environ["XTENSA_BINUTILS"]
    found = shutil.which("xtensa-esp32s3-elf-nm")
    if not found:
        sysroot = subprocess.check_output(["rustc", "+esp", "--print", "sysroot"], text=True).strip()
        hits = glob.glob(sysroot + "/xtensa-esp-elf/*/xtensa-esp-elf/bin/xtensa-esp32s3-elf-nm")
        found = hits[0] if hits else None
    if not found:
        sys.exit("xtensa-esp32s3-elf-nm not found: set XTENSA_BINUTILS=/path/xtensa-esp32s3-elf-")
    return found[: -len("nm")]

BIN = toolchain_prefix()
data = open(elf, "rb").read()

# sections (addr, offset, size) of the loaded ones
secs = []
for line in subprocess.check_output([BIN + "readelf", "-S", "-W", elf], text=True).splitlines():
    m = re.match(r"\s*\[\s*\d+\]\s+(\S+)\s+PROGBITS\s+([0-9a-f]+)\s+([0-9a-f]+)\s+([0-9a-f]+)", line)
    if m and int(m.group(2), 16) >= 0x3c000000:
        secs.append((int(m.group(2), 16), int(m.group(3), 16), int(m.group(4), 16)))

def read(addr, n):
    for a, off, size in secs:
        if a <= addr < a + size:
            return data[off + addr - a: off + addr - a + n]
    return b""

def word(addr):
    b = read(addr, 4)
    return struct.unpack("<I", b)[0] if len(b) == 4 else None

syms = []
for line in subprocess.check_output([BIN + "nm", "-C", "-S", "-n", elf], text=True, errors="replace").splitlines():
    p = line.split(" ", 3)
    if len(p) == 4 and p[2] in ("T", "t", "W", "w"):
        try:
            syms.append((int(p[0], 16), int(p[1], 16), p[3]))
        except ValueError:
            pass
syms.sort()
addrs = [s[0] for s in syms]
def func_at(a):
    i = bisect.bisect_right(addrs, a) - 1
    return i if i >= 0 and a < syms[i][0] + max(syms[i][1], 1) else None

frame = {}
edges = collections.defaultdict(set)
sext = lambda v, bits: v - (1 << bits) if v & (1 << (bits - 1)) else v
for i, (start, size, name) in enumerate(syms):
    code = read(start, size)
    pc, lit = 0, {}
    while pc + 2 <= len(code):
        b0 = code[pc]
        op0 = b0 & 0xF
        if op0 >= 8:               # density (narrow) instruction, 2 bytes
            pc += 2
            continue
        if pc + 3 > len(code):
            break
        v = code[pc] | code[pc + 1] << 8 | code[pc + 2] << 16
        at = start + pc
        n = (v >> 4) & 3
        if op0 == 6 and n == 3 and ((v >> 8) & 0xF) == 1 and pc == 0:        # entry a1, imm12*8
            frame[i] = (v >> 12) * 8
        elif op0 == 5:                                                         # call0/4/8/12
            tgt = ((at & ~3) + 4 + (sext(v >> 6, 18) << 2))
            g = func_at(tgt)
            if g is not None and g != i:
                edges[i].add(g)
        elif op0 == 6 and n == 0:                                              # j
            tgt = at + 4 + sext(v >> 6, 18)
            g = func_at(tgt)
            if g is not None and g != i:
                edges[i].add(g)
        elif op0 == 1:                                                         # l32r at, offset
            reg = (v >> 4) & 0xF
            imm = (v >> 8) & 0xFFFF
            la = ((at + 3) & ~3) + ((0xFFFF0000 | imm) << 2) - (1 << 32)
            w = word(la & 0xFFFFFFFF)
            lit[reg] = w
        elif op0 == 0 and (v >> 12) & 0xF == 0 and (v >> 16) & 0xFF in (0xC0, 0xD0, 0xE0, 0xF0) and False:
            pass
        elif op0 == 0 and ((v >> 4) & 0xF) == 0 and ((v >> 8) & 0xF) == 0 and ((v >> 12) & 0xF) == 0 and ((v >> 16) & 0xF) == 0 and False:
            pass
        else:
            # callx0/4/8/12 as: CALLX is op0=0, op1=0, op2=0, r=0, m=3, n=0..3, s=reg  -> bytes: [n<<4|0, s<<4|0?..]
            if op0 == 0 and (v >> 16) & 0xFF == 0 and ((v >> 12) & 0xF) == 0 and ((v >> 6) & 3) == 3:
                reg = (v >> 8) & 0xF
                tgt = lit.get(reg)
                if tgt:
                    g = func_at(tgt)
                    if g is not None and g != i:
                        edges[i].add(g)
        pc += 3

roots = [i for i, s in enumerate(syms) if re.search(root_pat, s[2])]
if not roots:
    sys.exit("no root")
order, seen, onstack = [], set(), set()
for r in roots:
    if r in seen:
        continue
    st = [(r, iter(sorted(edges[r])))]
    seen.add(r); onstack.add(r)
    while st:
        node, it = st[-1]
        for v in it:
            if v in onstack or v in seen:
                continue
            seen.add(v); onstack.add(v); st.append((v, iter(sorted(edges[v])))); break
        else:
            onstack.discard(node); order.append(node); st.pop()
order.reverse()
depth = {r: frame.get(r, 0) for r in roots}
for u in order:
    if u not in depth:
        continue
    for v in edges[u]:
        d = depth[u] + frame.get(v, 0)
        if d > depth.get(v, -1) and v not in roots and v in seen:
            depth[v] = d
print(f"functions {len(syms)}, with a frame {len(frame)}, edges {sum(len(e) for e in edges.values())}; root frame {frame.get(roots[0],0)}; reached {len(depth)}; deepest {max(depth.values())}")
for pat in patterns:
    hits = sorted(((depth[i], syms[i][2]) for i in depth if re.search(pat, syms[i][2])), reverse=True)[:3]
    print(f"[{pat}]")
    for d, n in hits:
        print(f"  {d:6d}  {n[:140]}")
