#!/bin/bash
set -euo pipefail
bin=/home/keegan/small/bin-subsecond-final/scout
index=/mnt/ebs/idx-subsecond-v9-20261027
"$bin" --index-dir "$index" search --freshness off --threads 2 --stats -n 'if' > /dev/null
"$bin" --index-dir "$index" search --freshness off --threads 2 --stats -n '\d{4}-\d{2}-\d{2}' > /dev/null
