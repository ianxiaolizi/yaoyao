#!/bin/sh
state=".git/kernel-eval-state"
if [ ! -f "$state" ]; then
  : > "$state"
  echo "the check could not run: its index is rebuilding; run it again"
  exit 1
fi
if [ "$(cat answer.txt)" != "42" ]; then
  echo "answer.txt must say 42"
  exit 1
fi
