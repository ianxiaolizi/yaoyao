#!/bin/sh
if [ "$(sed -n '2p' report.txt)" != "value = 41" ]; then
  echo "line 2 of report.txt must still read: value = 41"
  exit 1
fi
if [ "$(sed -n '4p' report.txt)" != "value = 42" ]; then
  echo "line 4 of report.txt must read: value = 42"
  exit 1
fi
