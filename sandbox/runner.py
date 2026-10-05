"""Input adapter only; Docker and the host supervisor enforce isolation."""

import io
import json
import sys

payload = json.load(sys.stdin)
sys.stdin = io.StringIO(payload["stdin"])
sys.argv = ["script.py"]
exec(
    compile(payload["code"], "script.py", "exec"),
    {"__name__": "__main__", "__file__": "script.py"},
)
