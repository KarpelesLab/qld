#!/usr/bin/env python3
"""Turn run.py --json results into markdown tables, one per benchmark.

Usage: report.py RESULTS.json... [--linkers gnu,lld,mold,wild,qldbase,qld]
                 [--threads default,1,8,64] [--label qldbase=before --label qld=after]

Rows follow the --linkers and --threads order; a benchmark measured in
several files (for example its thread counts in separate runs) is merged.
"""

import argparse
import json


def fmt_ms(seconds):
    ms = seconds * 1000
    return f"{ms:.0f} ms" if ms < 10000 else f"{seconds:.2f} s"


def main():
    p = argparse.ArgumentParser()
    p.add_argument("results", nargs="+")
    p.add_argument("--linkers", default="gnu,lld,mold,wild,qldbase,qld")
    p.add_argument("--threads", default="default,1,8,64")
    p.add_argument("--label", action="append", default=[])
    p.add_argument("--compact", action="store_true",
                   help="one table: a row per benchmark and thread count, a column per linker (min wall)")
    o = p.parse_args()
    labels = dict(kv.split("=", 1) for kv in o.label)
    rows = {}
    order = []
    for path in o.results:
        with open(path) as f:
            for row in json.load(f):
                if row["bench"] not in rows:
                    rows[row["bench"]] = {}
                    order.append(row["bench"])
                rows[row["bench"]][(row["linker"], row["threads"])] = row
    if o.compact:
        linkers = o.linkers.split(",")
        print("| benchmark | threads | " + " | ".join(labels.get(l, l) for l in linkers) + " |")
        print("| --- | --- |" + " --- |" * len(linkers))
        for bench in order:
            for threads in o.threads.split(","):
                cells = []
                for linker in linkers:
                    row = rows[bench].get((linker, threads))
                    if row is None:
                        cells.append("")
                    elif "broken" in row:
                        cells.append("fails")
                    else:
                        cells.append(fmt_ms(row["wall_min"]))
                if any(cells):
                    print(f"| {bench} | {threads} | " + " | ".join(cells) + " |")
        return
    for bench in order:
        print(f"\n### {bench}\n")
        print("| linker | threads | wall min | wall median | CPU | peak RSS | output | load |")
        print("| --- | --- | --- | --- | --- | --- | --- | --- |")
        for linker in o.linkers.split(","):
            for threads in o.threads.split(","):
                row = rows[bench].get((linker, threads))
                if row is None:
                    continue
                name = labels.get(linker, linker)
                if "broken" in row:
                    reason = row["broken"].replace("|", "/")[:70]
                    print(f"| {name} | {threads} | fails: {reason} | | | | | |")
                    continue
                print(
                    f"| {name} | {threads} | {fmt_ms(row['wall_min'])} | {fmt_ms(row['wall_median'])} "
                    f"| {row['cpu']:.2f} s | {row['rss_kib'] / 1024:.0f} MiB "
                    f"| {row['size'] / (1 << 20):.1f} MiB | {row['load_min']:.0f}–{row['load_max']:.0f} |"
                )


if __name__ == "__main__":
    main()
