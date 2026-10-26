#!/usr/bin/env python3
"""FreeBSD `usbconfig -lv` (dump_all_config_desc) output -> the raw USB descriptors a host reads,
as Android's UsbDeviceConnection.getRawDescriptors() returns them: the 18-byte device descriptor
followed by every configuration descriptor with its interfaces, endpoints and class-specific
descriptors, in order. Each configuration's bytes must add up to its wTotalLength, or this fails.

Standard descriptors are rebuilt from their fields; class-specific ones ("Additional Descriptor")
come from their RAW dump. The device descriptor's bMaxPacketSize0 isn't in usbconfig's summary
line: it is written as 0x40 (what full- and high-speed audio devices use).

Usage: usbconfig-to-raw.py <dump.txt> <out.bin>
"""
import re
import sys

text = open(sys.argv[1]).read()
lines = text.splitlines()


def fields(block):
    out = {}
    for l in block:
        m = re.match(r"\s*(\w+)\s*=\s*(0x[0-9a-fA-F]+)", l)
        if m and m.group(1) not in out:
            out[m.group(1)] = int(m.group(2), 16)
    return out


head = next(l for l in lines if "idVendor=" in l)
hv = {k: int(v, 16) for k, v in re.findall(r"(\w+)=(0x[0-9a-fA-F]+)", head)}
dev = bytes([18, 1]) + hv["bcdUSB"].to_bytes(2, "little") + bytes(
    [hv["bDeviceClass"], hv["bDeviceSubClass"], hv["bDeviceProtocol"], 0x40]
) + hv["idVendor"].to_bytes(2, "little") + hv["idProduct"].to_bytes(2, "little") + hv["bcdDevice"].to_bytes(
    2, "little"
) + bytes([hv["iManufacturer"], hv["iProduct"], hv["iSerialNumber"], hv["bNumConfigurations"]])

# Split into blocks at blank lines; classify each
blocks, cur = [], []
for l in lines:
    if l.strip() == "":
        if cur:
            blocks.append(cur)
            cur = []
    else:
        cur.append(l)
if cur:
    blocks.append(cur)

configs, out, total = [], None, None
for b in blocks:
    joined = "\n".join(b)
    if "RAW dump:" in joined:
        raw = []
        started = False
        for l in b:
            if "RAW dump:" in l:
                started = True
                continue
            if started:
                m = re.match(r"\s*0x[0-9a-fA-F]+\s*\|\s*(.*)", l)
                if m:
                    raw += [int(x, 16) for x in re.findall(r"0x([0-9a-fA-F]+)", m.group(1))]
        f = fields(b)
        if raw and raw[0] != f.get("bLength", raw[0]):
            sys.exit(f"raw dump length {raw[0]} != bLength {f.get('bLength')}")
        out += bytes(raw)
        continue
    f = fields(b)
    t = f.get("bDescriptorType")
    if t == 2:  # configuration
        if out is not None:
            configs.append((out, total))
        total = f["wTotalLength"]
        out = bytearray([9, 2]) + f["wTotalLength"].to_bytes(2, "little") + bytes(
            [f["bNumInterfaces"], f["bConfigurationValue"], f["iConfiguration"], f["bmAttributes"], f["bMaxPower"]]
        )
    elif t == 4:  # interface
        out += bytes([9, 4, f["bInterfaceNumber"], f["bAlternateSetting"], f["bNumEndpoints"], f["bInterfaceClass"],
                      f["bInterfaceSubClass"], f["bInterfaceProtocol"], f["iInterface"]])
    elif t == 5:  # endpoint
        n = f["bLength"]
        e = bytes([n, 5, f["bEndpointAddress"], f["bmAttributes"]]) + f["wMaxPacketSize"].to_bytes(2, "little") + bytes(
            [f["bInterval"]])
        if n == 9:
            e += bytes([f["bRefresh"], f["bSynchAddress"]])
        out += e
if out is not None:
    configs.append((out, total))
blob = bytearray(dev)
for i, (c, t) in enumerate(configs):
    if len(c) != t:
        sys.exit(f"configuration {i}: {len(c)} bytes, wTotalLength says {t}")
    blob += c
open(sys.argv[2], "wb").write(blob)
print(f"{len(configs)} configurations, {len(blob)} bytes; each matches its wTotalLength")
