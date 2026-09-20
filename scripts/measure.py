#!/usr/bin/env python3
"""Compare resident memory of br0x vs firefox, summing full process trees.

Usage: measure.py [root-name ...]   (default: firefox br0x)
A root is any process whose executable name matches; its whole descendant
tree (bwrap wrappers, WebKitWebProcess, WebKitNetworkProcess, ...) is summed.
"""
import os
import sys


def read_rss(pid):
    try:
        with open(f"/proc/{pid}/smaps_rollup") as f:
            for line in f:
                if line.startswith("Pss:"):
                    return int(line.split()[1])
    except OSError:
        pass
    return 0


def read_status(pid):
    try:
        with open(f"/proc/{pid}/status") as f:
            status = {}
            for line in f:
                key, _, value = line.partition(":")
                status[key.strip()] = value.strip()
        return status
    except OSError:
        return None


def proc_table():
    table = {}
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        pid = int(entry)
        status = read_status(pid)
        if status is None:
            continue
        ppid = int(status.get("PPid", 0))
        rss_kb = read_rss(pid)
        name = status.get("Name", "")
        table[pid] = (ppid, rss_kb, name)
    return table


def tree_rss(table, root):
    total = 0
    count = 0
    stack = [root]
    while stack:
        pid = stack.pop()
        _, rss_kb, _ = table.get(pid, (0, 0, ""))
        total += rss_kb
        count += 1
        stack.extend(child for child, (ppid, _, _) in table.items() if ppid == pid)
    return total, count


def main():
    names = sys.argv[1:] or ["firefox", "br0x"]
    table = proc_table()
    mem_avail = None
    with open("/proc/meminfo") as f:
        for line in f:
            if line.startswith("MemAvailable"):
                mem_avail = int(line.split()[1]) // 1024
                break
    if mem_avail is not None:
        print(f"MemAvailable: {mem_avail} MB")
    for name in names:
        roots = [
            pid for pid, (_, _, pname) in table.items() if pname.startswith(name[:15])
        ]
        roots = [pid for pid in roots if table[pid][2] == name[:15]]
        total, count = 0, 0
        for root in roots:
            sub_total, sub_count = tree_rss(table, root)
            total += sub_total
            count += sub_count
        print(f"{name}: {count} procs, {total / 1024:.1f} MB PSS")


if __name__ == "__main__":
    main()
