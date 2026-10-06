"""Input adapter and diagnostics only; Docker and the host supervisor enforce isolation."""

import io
import json
import linecache
import sys
import traceback


def script_error(exc_type, exc_value, exc_traceback, adapter_filename=__file__):
    """Show user source and exception chains, omitting this adapter's frames."""
    error = traceback.TracebackException(exc_type, exc_value, exc_traceback)
    pending = [error]
    while pending:
        current = pending.pop()
        current.stack = traceback.StackSummary.from_list(
            frame for frame in current.stack if frame.filename != adapter_filename
        )
        pending.extend(item for item in (current.__cause__, current.__context__) if item is not None)
        pending.extend(getattr(current, "exceptions", None) or [])
    sys.stderr.write("".join(error.format()))


payload = json.load(sys.stdin)
source = payload["code"]
# Compiled stdin has no file on disk for traceback's normal source lookup.
linecache.cache["script.py"] = (len(source), None, source.splitlines(keepends=True), "script.py")
sys.excepthook = script_error
sys.stdin = io.StringIO(payload["stdin"])
sys.argv = ["script.py"]
exec(
    compile(source, "script.py", "exec"),
    {"__name__": "__main__", "__file__": "script.py"},
)
