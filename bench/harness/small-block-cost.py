"""Report the cache cost of storing small v9 LZ4 blocks raw; does not read payloads."""
import json
from pathlib import Path
import struct
import sys

if len(sys.argv) != 2:
    raise SystemExit("usage: small-block-cost.py <v9-shards-directory>")
paths = sorted(Path(sys.argv[1]).glob("*.native"))
if not paths:
    raise ValueError("no native shards")
totals = {limit: {"blocks": 0, "extra_bytes": 0, "raw_bytes": 0} for limit in (1024, 2048, 4096, 8192)}
for path in paths:
    with path.open("rb") as source:
        header = source.read(112)
        if len(header) != 112 or header[:8] != b"SCOUTNI9" or struct.unpack_from("<I", header, 8)[0] != 9:
            raise ValueError(f"expected v9: {path}")
        blocks = struct.unpack_from("<I", header, 24)[0]
        offset = struct.unpack_from("<Q", header, 48)[0]
        source.seek(offset)
        payload = source.read(blocks * 12)
        if len(payload) != blocks * 12:
            raise ValueError(f"truncated block table: {path}")
        for stored, raw, lines in struct.iter_unpack("<III", payload):
            if stored < raw:
                for limit, row in totals.items():
                    if raw < limit:
                        row["blocks"] += 1
                        row["extra_bytes"] += raw - stored
                        row["raw_bytes"] += raw
print(json.dumps(totals, indent=2))
