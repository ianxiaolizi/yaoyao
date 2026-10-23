#!/bin/sh
[ "$(cat answer.txt)" = "42" ] || { echo "answer.txt must say 42"; exit 1; }
[ "$(cat flag.txt)" = "on" ] || { echo "flag.txt must say on"; exit 1; }
