"""How fast a pipe carries a saved scout output into a Python capture_output reader (the
reader ab_scout.py and scout_only.py use), by the producer's write size: the floor under any
query that prints that much (E26). The outputs are made first with plain redirects, e.g.
`scout search -n -- if > <dir>/if.out` and `scout search -n -F -- return > <dir>/return.out`.
    pipe_ceiling.py run <dir>                 times every <name>.out in <dir>
    pipe_ceiling.py produce <file> <bytes>    (internal) writes <file> to stdout"""
import glob, os, statistics, subprocess, sys, time

if len(sys.argv) == 4 and sys.argv[1] == "produce":
    data = memoryview(open(sys.argv[2], "rb").read())
    n = int(sys.argv[3])
    fd = sys.stdout.fileno()
    t = time.perf_counter()
    at = 0
    while at < len(data):
        # A pipe write may take fewer bytes than offered; carry on from where it stopped.
        at += os.write(fd, data[at : at + n])
    print(f"{(time.perf_counter() - t) * 1000:.0f}", file=sys.stderr)
elif len(sys.argv) == 3 and sys.argv[1] == "run":
    files = sorted(glob.glob(os.path.join(sys.argv[2], "*.out")))
    if not files:
        sys.exit(f"no *.out files in {sys.argv[2]}")
    for path in files:
        for chunk in (64 << 10, 1 << 20, 8 << 20):
            ts = []
            for _ in range(3):
                p = subprocess.run([sys.executable, "-I", __file__, "produce", path, str(chunk)], capture_output=True)
                if p.returncode != 0 or len(p.stdout) != os.path.getsize(path):
                    sys.exit(f"producer failed on {path}: exit {p.returncode}, {len(p.stdout)} bytes read; {p.stderr[-300:]!r}")
                ts.append(float(p.stderr))
            name = os.path.basename(path)[:-4]
            print(f"{name} {os.path.getsize(path) >> 20} MiB, writes of {chunk >> 10} KiB: {statistics.median(ts):.0f} ms writing")
else:
    sys.exit(__doc__)
