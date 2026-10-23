#!/bin/sh
if grep -rq "value: 41" notes; then
  echo "a note disagrees with sum.txt"
  exit 1
fi
