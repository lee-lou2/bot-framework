//! 가짜 `container` 실행 파일(파이썬 스크립트): 상태 파일(`state.json`)을 읽고 고쳐 쓰며 부른 명령을 `calls.log`에 적는다.

pub(super) const FAKE: &str = r#"#!/usr/bin/env python3
import fcntl, json, os, sys, time
here = os.path.dirname(os.path.realpath(__file__))
path = os.path.join(here, "state.json")
# 같은 상태 파일을 동시에 여러 번 부르므로(닻 감시·정리) 읽고 고쳐 쓰는 동안은 한 번에 하나만 하고, 쓸 때는 통째로 바꿔 놓는다
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
    # 이 컨테이너를 띄운 `run` 클라이언트가 아직 살아 있는가(끝내라는 명령이 오기 전에 클라이언트가 먼저 끝났는지 본다)
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
    # 진짜 CLI처럼: 있으면 0과 JSON, 없으면 1과 «container not found». `inspect_unknown`이면 서비스가 응답하지 않는 것처럼 «없음»도 «있음»도 아니게 실패한다
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
        # 첫 지우기보다 늦게 도착하는 «만들기» 요청: 지운 지 N ms 뒤에 따로 도는 프로세스가 컨테이너를 목록에 더한다(그때는 지울 것이 없었다).
        # 잠금은 포크 전에 놓는다(자식이 물려받은 잠금을 쥔 채 자면 다른 명령이 그동안 막힌다)
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
        # 닻 VM: 끝나지 않고 기다린다(실패 설정이면 곧바로 끝난다). 떠 있는 동안 컨테이너 목록에 라벨과 함께 보인다
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
        # 모델 VM도 목록에 보인다(클라이언트가 죽어도 남는다: `--rm`을 지켜 줄 클라이언트가 없다)
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
