import json, base64, urllib.request, sys, time

PORTAINER = "http://192.168.0.21:9000"
API_KEY = "ptr_ZYi4rjc6DQIzAH3joO8th827Rxq38vE2b9NjKQPPkrQ="
ENDPOINT = "13"
ROOT = r"D:\GitHub\nd-organizer"


def api(method, path, data=None):
    url = PORTAINER + "/api/endpoints/" + ENDPOINT + path
    headers = {"X-API-Key": API_KEY}
    body = None
    if data is not None:
        headers["Content-Type"] = "application/json"
        body = json.dumps(data).encode()
    req = urllib.request.Request(url, data=body, headers=headers, method=method)
    resp = urllib.request.urlopen(req)
    raw = resp.read()
    if not raw:
        return None
    return json.loads(raw)


def get_cid(name):
    containers = api("GET", "/docker/containers/json")
    for c in containers:
        if name in c["Names"][0]:
            return c["Id"]
    return None


def push_file(container_name, local_path, remote_path):
    cid = get_cid(container_name)
    if not cid:
        print("SKIP: " + container_name + " not found")
        return

    with open(local_path, "rb") as f:
        data = f.read()
    b64 = base64.b64encode(data).decode()

    # Write via python inside container to avoid shell quoting issues
    cmd = "python3 -c \"import base64; open('" + remote_path + "','wb').write(base64.b64decode('" + b64 + "'))\""

    exec_resp = api("POST", "/docker/containers/" + cid + "/exec", {
        "AttachStdout": True,
        "AttachStderr": True,
        "Cmd": ["sh", "-c", cmd]
    })

    exec_id = exec_resp["Id"]
    urllib.request.urlopen(urllib.request.Request(
        PORTAINER + "/api/endpoints/" + ENDPOINT + "/docker/exec/" + exec_id + "/start",
        data=json.dumps({"Detach": False, "Tty": False}).encode(),
        headers={"X-API-Key": API_KEY, "Content-Type": "application/json"},
        method="POST"
    )).read()

    resp = api("POST", "/docker/containers/" + cid + "/restart")
    print("OK: " + container_name + " <- " + local_path + " (" + str(len(data)) + " bytes)")
    time.sleep(5)


containers = [
    ("nd-organizer-acoustid", "acoustid/server.py"),
    ("nd-organizer-webhook", "webhook/server.py"),
    ("nd-organizer-proxy", "proxy/server.py"),
    ("nd-organizer-mysql", "mysql/server.py"),
    ("nd-organizer-essentia", "essentia/server.py"),
]

for name, path in containers:
    push_file(name, ROOT + "/" + path, "/app/server.py")

print("All sidecars updated. Waiting 15s for startup...")
time.sleep(15)

# Health checks
for port, name in [(8097, "AcoustID"), (8099, "Webhook"), (4534, "Proxy"), (8098, "MySQL"), (8101, "Essentia")]:
    try:
        resp = urllib.request.urlopen("http://192.168.0.21:" + str(port) + "/health", timeout=5)
        body = json.loads(resp.read())
        print(name + ": OK - " + body.get("service", "?"))
    except Exception as e:
        print(name + ": FAIL - " + str(e))
