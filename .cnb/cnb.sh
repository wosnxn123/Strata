#!/bin/bash
# CNB helper — new host after the old instance died.
SSH="ssh -o BatchMode=yes -o ConnectTimeout=20 -o ServerAliveInterval=10 cnb-m7g-1jv62ki18-001.b26fc798-3fae-4801-a1f0-a571b4824804-lcu@cnb.space"
cmd="$1"
for i in 1 2 3 4 5; do
  $SSH "$cmd" && exit 0
  echo "[cnb retry $i]" >&2
  sleep $((i*4))
done
exit 255
