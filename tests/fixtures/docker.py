"""Deterministic Docker control-plane fixture. It never executes submitted code."""
import json
import sys
import time
from pathlib import Path

root = Path(__file__).parent
args = sys.argv[3:]  # --host unix://...
mode = (root / "mode").read_text()
image = "sha256:" + "a" * 64
state = root / "container.json"
with (root / "events").open("a") as log:
    log.write(json.dumps(args) + "\n")

def emit(value):
    print(json.dumps(value), flush=True)

if args[0] == "info":
    emit({"OSType": "linux", "MemoryLimit": True, "SwapLimit": True,
          "CpuCfsQuota": True, "PidsLimit": True,
          "SecurityOptions": ["name=seccomp,profile=builtin"]})
elif args[:2] == ["image", "inspect"]:
    emit([{"Id": image, "Os": "linux", "Config": {
        "User": "1000:1000", "Entrypoint": ["/usr/local/bin/python", "-I", "-B", "-u", "/opt/runner.py"]}}])
elif args[0] == "create":
    name = args[args.index("--name") + 1]
    labels = dict(args[i + 1].split("=", 1) for i, arg in enumerate(args) if arg == "--label")
    container = {
        "Name": "/" + name,
        "Config": {"Image": image, "User": "1000:1000", "WorkingDir": "/tmp", "Labels": labels,
                   "Entrypoint": ["/usr/local/bin/python"], "Cmd": ["-I", "-B", "-u", "/opt/runner.py"]},
        "HostConfig": {"ReadonlyRootfs": True, "Privileged": False, "NetworkMode": "none",
                       "CapDrop": ["ALL"], "SecurityOpt": ["no-new-privileges", "seccomp=builtin"],
                       "Memory": 268435456, "MemorySwap": 268435456, "NanoCpus": 1000000000,
                       "PidsLimit": 16, "ShmSize": 65536, "IpcMode": "private", "Init": True,
                       "LogConfig": {"Type": "none"}, "RestartPolicy": {"Name": "no"},
                       "Tmpfs": {"/tmp": "rw,noexec,nosuid,nodev,size=67108864,mode=1777"}},
        "Mounts": [], "State": {"Status": "created", "ExitCode": 0, "OOMKilled": False}}
    if mode == "policyfail":
        container["HostConfig"]["Memory"] = 0
    state.write_text(json.dumps(container))
    print("b" * 64)
    if mode == "createfail":
        sys.exit(1)
elif args[:2] == ["container", "inspect"]:
    if not state.exists():
        sys.exit(1)
    emit([json.loads(state.read_text())])
elif args[0] == "start":
    payload = json.load(sys.stdin)
    (root / "ready").write_text("ready")
    if mode == "hang":
        while state.exists():
            time.sleep(0.01)
        sys.exit(0)
    if mode == "output":
        sys.stdout.write("x" * 100000)
        sys.stdout.flush()
    elif mode == "error":
        print("ValueError: fixture", file=sys.stderr)
    elif mode == "oom":
        pass
    else:
        print("42")
    container = json.loads(state.read_text())
    container["State"] = {"Status": "exited", "ExitCode": 1 if mode == "error" else 137 if mode == "oom" else 0,
                          "OOMKilled": mode == "oom"}
    state.write_text(json.dumps(container))
elif args[0] == "rm":
    if mode == "cleanupfail":
        sys.exit(1)
    state.unlink(missing_ok=True)
elif args[0] == "ps":
    if state.exists():
        container = json.loads(state.read_text())
        print(container["Name"][1:] if "--format" in args else "b" * 64)
else:
    raise AssertionError(args)
