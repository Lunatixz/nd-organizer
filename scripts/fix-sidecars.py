import json, base64, urllib.request, time, io, tarfile

PORTAINER = "http://192.168.0.21:9000"
API_KEY = "ptr_ZYi4rjc6DQIzAH3joO8th827Rxq38vE2b9NjKQPPkrQ="
ENDPOINT = "13"
ROOT = r"D:\GitHub\nd-organizer"


def api(method, path, data=None, content_type=None):
    url = PORTAINER + "/api/endpoints/" + ENDPOINT + path
    headers = {"X-API-Key": API_KEY}
    if content_type:
        headers["Content-Type"] = content_type
    elif data is not None:
        headers["Content-Type"] = "application/json"
        data = json.dumps(data).encode()
    req = urllib.request.Request(url, data=data, headers=headers, method=method)
    resp = urllib.request.urlopen(req)
    raw = resp.read()
    if not raw:
        return None
    return json.loads(raw)


def get_cid(name):
    containers = api("GET", "/docker/containers/json?all=true")
    for c in containers:
        if name in c["Names"][0]:
            return c["Id"]
    return None


def copy_file_to_container(cid, local_path, remote_path, container_name):
    """Use Docker's /containers/{id}/archive endpoint to put a file."""
    # Create tar archive in memory with just the one file
    tar_buf = io.BytesIO()
    with tarfile.open(fileobj=tar_buf, mode="w") as tar:
        tar.add(local_path, arcname=remote_path.lstrip("/"))
    tar_data = tar_buf.getvalue()

    url = PORTAINER + "/api/endpoints/" + ENDPOINT + "/docker/containers/" + cid + "/archive?path=/"
    req = urllib.request.Request(url, data=tar_data, headers={
        "X-API-Key": API_KEY,
        "Content-Type": "application/x-tar",
    }, method="PUT")
    resp = urllib.request.urlopen(req)
    print("Copied: " + container_name + " <- " + local_path.split("/")[-1] + " (" + str(len(tar_data)) + " bytes)")


# Stop all containers
print("=== Stopping all sidecar containers ===")
cids = {}
for name in ["nd-organizer-acoustid", "nd-organizer-webhook", "nd-organizer-proxy", "nd-organizer-mysql", "nd-organizer-essentia"]:
    cid = get_cid(name)
    if cid:
        try:
            api("POST", "/docker/containers/" + cid + "/stop")
            print("  Stopped: " + name)
        except:
            print("  Already stopped: " + name)
        cids[name] = cid

time.sleep(3)

# Copy files while stopped (Docker archive API works on stopped containers)
print("\n=== Copying sidecar files ===")
pushes = [
    ("nd-organizer-acoustid", "acoustid/server.py", "/app/server.py"),
    ("nd-organizer-webhook", "webhook/server.py", "/app/server.py"),
    ("nd-organizer-proxy", "proxy/server.py", "/app/server.py"),
    ("nd-organizer-mysql", "mysql/server.py", "/app/server.py"),
    ("nd-organizer-essentia", "essentia/server.py", "/app/server.py"),
]

for name, src, dst in pushes:
    if name in cids:
        copy_file_to_container(cids[name], ROOT + "/" + src, dst, name)

# Start all
print("\n=== Starting all containers ===")
for name, cid in cids.items():
    try:
        api("POST", "/docker/containers/" + cid + "/start")
        print("  Started: " + name)
    except Exception as e:
        print("  Start failed: " + name + " - " + str(e))

print("\nWaiting 25s for startup...")
time.sleep(25)

# Health checks
print("\n=== Health Checks ===")
all_ok = True
for port, name in [(8097, "AcoustID"), (8099, "Webhook"), (4534, "Proxy"), (8098, "MySQL"), (8101, "Essentia")]:
    try:
        resp = urllib.request.urlopen("http://192.168.0.21:" + str(port) + "/health", timeout=5)
        body = json.loads(resp.read())
        print("  " + name + ": OK - " + body.get("service", "?"))
    except Exception as e:
        print("  " + name + ": FAIL - " + str(e))
        all_ok = False

if all_ok:
    print("\nAll sidecars healthy!")
else:
    print("\nSome sidecars unhealthy - check logs.")
