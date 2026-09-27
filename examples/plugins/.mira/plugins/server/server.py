import itertools
import sys
import time

for n in itertools.count(1):
    print(f"GET /api/items 200 ({n})")
    if n % 4 == 0:
        print(f"error: GET /api/items/{n} 500 upstream timed out", file=sys.stderr)
    time.sleep(1)
