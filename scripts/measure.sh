#!/usr/bin/env bash
# Compare resident memory of br0x vs firefox. Same method for both.
# Usage: ./scripts/measure.sh [pattern...] (default: firefox br0x)
set -u
pats=("$@")
if [ "${#pats[@]}" -eq 0 ]; then
  pats=(firefox br0x)
fi
echo "MemAvailable: $(awk '/MemAvailable/{print $2/1024" MB"}' /proc/meminfo)"
for pat in "${pats[@]}"; do
  ps -eo rss,args \
    | grep -i "[${pat:0:1}]${pat:1}" \
    | awk -v name="$pat" '{s+=$1; n++} END {printf "%s: %d procs, %.1f MB RSS\n", name, n+0, s/1024}'
done
