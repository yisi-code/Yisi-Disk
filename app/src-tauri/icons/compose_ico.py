"""合成 icon.ico：48 px 及以下的几层取自小尺寸图标，64 px 及以上取自大尺寸图标。

用法：python compose_ico.py <小尺寸的 icon.ico> <大尺寸的 icon.ico> <输出的 icon.ico>
"""
import struct, sys
def read(path):
    data = open(path, "rb").read()
    _, kind, count = struct.unpack_from("<HHH", data, 0)
    assert kind == 1
    layers = []
    for i in range(count):
        w, h, colors, res, planes, bpp, size, offset = struct.unpack_from("<BBBBHHII", data, 6 + 16 * i)
        layers.append(((w or 256), (w, h, colors, res, planes, bpp), data[offset:offset + size]))
    return layers
small = [l for l in read(sys.argv[1]) if l[0] <= 48]
large = [l for l in read(sys.argv[2]) if l[0] > 48]
layers = sorted(small + large, key=lambda l: l[0])
out = bytearray(struct.pack("<HHH", 0, 1, len(layers)))
offset = 6 + 16 * len(layers)
for size, head, blob in layers:
    out += struct.pack("<BBBBHHII", *head, len(blob), offset)
    offset += len(blob)
for _, _, blob in layers:
    out += blob
open(sys.argv[3], "wb").write(out)
print("layers:", [(l[0], "small" if l[0] <= 48 else "main") for l in layers])
