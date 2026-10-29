#!/bin/bash
# Summarise /tmp/e58p/<name>.data: per event, the top user-space frames above the issue.
set -uo pipefail
PERF=/usr/lib/linux-tools/6.8.0-147-generic/perf
for f in /tmp/e58p/*.data; do
  echo "== $f"
  "$PERF" script -i "$f" -F event,ip,sym 2>/dev/null > /tmp/e58p/s.txt
  grep -c -E '^ *block:block_rq_issue' /tmp/e58p/s.txt
  grep -c -E '^ *major-faults' /tmp/e58p/s.txt
  # For each block_rq_issue sample: the kernel entry (fault vs madvise vs read) and the
  # first scout frame.
  awk '
    /^ *[a-z-]+:?[a-z_]*:? *$/ || /block:block_rq_issue|major-faults/ { if (ev != "") print ev "\t" k "\t" u; ev = $1; k = ""; u = ""; next }
    NF >= 2 {
      s = $2
      if (k == "" && (s ~ /asm_exc_page_fault|do_madvise|ksys_read|ksys_pread|__x64_sys_openat|__x64_sys_statx|__x64_sys_getdents|__x64_sys_newfstatat|filemap_fault|__x64_sys_/)) k = s
      if (u == "" && s ~ /scout|memchr|regex|aho/) u = s
    }
    END { if (ev != "") print ev "\t" k "\t" u }
  ' /tmp/e58p/s.txt | sort | uniq -c | sort -rn | head -25
done
