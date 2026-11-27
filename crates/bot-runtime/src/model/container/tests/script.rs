//! Fake `container` CLI Python script backed by `state.json` and `calls.log`.

pub(super) const FAKE: &str = r#"#!/usr/bin/env python3
import fcntl, json, os, sys, time
here = os.path.dirname(os.path.realpath(__file__))
path = os.path.join(here, "state.json")
lock = open(os.path.join(here, "state.lock"), "a")
fcntl.flock(lock, fcntl.LOCK_EX)
st = json.load(open(path))
def release():
    fcntl.flock(lock, fcntl.LOCK_UN)
def save():
    tmp = "%s.%d" % (path, os.getpid())
    with open(tmp, "w") as f:
        json.dump(st, f)
    os.replace(tmp, path)
def client_alive(name):
    try:
        os.kill(int(open(os.path.join(here, "pid-" + name)).read()), 0)
        return True
    except (OSError, ValueError):
        return False
argv = sys.argv[1:]
with open(os.path.join(here, "calls.log"), "a") as log:
    log.write(json.dumps({"argv": argv, "env": {k: v for k, v in os.environ.items() if k in st.get("watch", [])}, "keys": sorted(os.environ.keys())}) + "\n")
cmd = argv[0] if argv else ""
if cmd == "network":
    if argv[1] == "inspect":
        if st.get("inspect_hangs"):
            release()
            time.sleep(30)
            sys.exit(1)
        n = st["networks"].get(argv[2])
        if n is None:
            sys.exit(1)
        print(json.dumps([{"configuration": {"mode": n["mode"]}, "status": {"ipv4Gateway": n.get("gateway", "")}}]))
    elif argv[1] == "create":
        if st.get("create_fails"):
            sys.exit(1)
        st["networks"][argv[-1]] = {"mode": st.get("create_mode", "hostOnly"), "gateway": "192.168.99.1"}
        save()
elif cmd == "image":
    ref = argv[2]
    digest = None
    for tag, d in st["images"].items():
        last = tag.split("/")[-1]
        name = tag[: len(tag) - len(last)] + last.split(":")[0]
        if ref == tag or ref == name + "@" + d:
            digest = d
    if digest is None:
        sys.exit(1)
    print(json.dumps([{"configuration": {"descriptor": {"digest": digest}}}]))
elif cmd == "inspect":
    if st.get("inspect_unknown"):
        sys.stderr.write("Error: apiserver unreachable\n")
        sys.exit(2)
    if any(c["configuration"]["id"] == argv[1] for c in st["containers"]):
        print(json.dumps([{"configuration": {"id": argv[1]}}]))
    else:
        sys.stderr.write("Error: container not found: %s\n" % argv[1])
        sys.exit(1)
elif cmd == "ls":
    if st.get("ls_fails"):
        sys.exit(1)
    print(json.dumps(st["containers"]))
elif cmd == "kill":
    if st.get("kill_hangs"):
        release()
        time.sleep(30)
        sys.exit(1)
    st["killed"].append(argv[-1])
    st.setdefault("alive_at_remove", []).append(client_alive(argv[-1]))
    pid_path = os.path.join(here, "pid-" + argv[-1])
    if os.path.exists(pid_path):
        try:
            os.kill(int(open(pid_path).read()), 15)
        except OSError:
            pass
    st["containers"] = [c for c in st["containers"] if not (c.get("anchor") and c["configuration"]["id"] == argv[-1])]
    save()
elif cmd == "delete":
    st.setdefault("alive_at_remove", []).append(client_alive(argv[-1]))
    if not st.get("delete_stuck"):
        st["containers"] = [c for c in st["containers"] if c["configuration"]["id"] != argv[-1]]
    st["deleted"].append(argv[-1]); save()
    if st.get("late_after_delete_ms") and not st.get("late_fired"):
        # Release flock before fork so the sleeping child does not block subsequent CLI commands.
        st["late_fired"] = True
        save()
        release()
        if os.fork() == 0:
            os.setsid()
            time.sleep(st["late_after_delete_ms"] / 1000.0)
            fcntl.flock(lock, fcntl.LOCK_EX)
            st = json.load(open(path))
            st["containers"].append({"configuration": {"id": argv[-1], "labels": {}}, "status": {"state": "stopped"}})
            save()
            release()
            os._exit(0)
elif cmd == "run":
    name = argv[argv.index("--name") + 1] if "--name" in argv else ""
    open(os.path.join(here, "pid-" + name), "w").write(str(os.getpid()))
    if any(a.startswith("bot.owner=anchor.") for a in argv):
        if st.get("anchor_fails"):
            sys.exit(1)
        labels = {}
        for i, a in enumerate(argv):
            if a == "--label":
                k, _, v = argv[i + 1].partition("=")
                labels[k] = v
        st["containers"].append({"anchor": True, "configuration": {"id": name, "labels": labels}, "status": {"state": "running"}})
        save()
        release()
        time.sleep(86400)
        sys.exit(0)
    record = {"configuration": {"id": name, "labels": {}}, "status": {"state": "running"}}
    if st.get("register"):
        st["containers"].append(record)
        save()
    release()
    stdin = sys.stdin.read() if "-i" in argv else None
    with open(os.path.join(here, "stdin.txt"), "w") as f:
        f.write(stdin or "")
    for line in st.get("lines", []):
        print(line, flush=True)
    if st.get("big_line"):
        sys.stdout.write("x" * st["big_line"]); sys.stdout.flush()
    if st.get("hang"):
        time.sleep(60)
    sys.exit(st.get("exit", 0))
"#;
