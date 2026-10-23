# The check's first line is the failure a later turn asks about; the list under
# it is what makes the failed call itself a large enough message that a
# compaction across turns has to summarize it rather than carry it whole. The
# failures record keeps the first line, which is where the expected value is.
cd "$(dirname "$0")" || exit 1
expected=42
if [ ! -f answer.txt ]; then
    echo "assertion failed: expected $expected, got missing"
    echo "records not verified:"
    while IFS= read -r line; do
        echo "unverified: $line"
    done < data/records.csv
    exit 1
fi
answer=$(tr -d '[:space:]' < answer.txt)
if [ "$answer" != "$expected" ]; then
    echo "assertion failed: expected $expected, got $answer"
    while IFS= read -r line; do
        echo "unverified: $line"
    done < data/records.csv
    exit 1
fi
exit 0
