"""Deploy genre model to Essentia sidecar."""
import urllib.request, json, sys, tarfile, io, os, time

API_URL = 'http://192.168.0.21:9000'
API_KEY = 'ptr_ZYi4rjc6DQIzAH3joO8th827Rxq38vE2b9NjKQPPkrQ='
CONTAINER = 'nd-organizer-essentia'
EP = '13'

model_dir = os.path.join(os.path.dirname(__file__), 'models')
os.makedirs(model_dir, exist_ok=True)

# Check MODEL_DIR in container
def run_cmd(cmd):
    exec_body = json.dumps({'AttachStdout': True, 'AttachStderr': True, 'Cmd': ['sh', '-c', cmd]}).encode()
    url = f'{API_URL}/api/endpoints/{EP}/docker/containers/{CONTAINER}/exec'
    req = urllib.request.Request(url, data=exec_body, headers={'X-API-Key': API_KEY, 'Content-Type': 'application/json'}, method='POST')
    r = urllib.request.urlopen(req, timeout=10)
    exec_id = json.loads(r.read().decode()).get('Id', '')
    start_body = json.dumps({'Detach': False, 'Tty': True}).encode()
    url2 = f'{API_URL}/api/endpoints/{EP}/docker/exec/{exec_id}/start'
    req2 = urllib.request.Request(url2, data=start_body, headers={'X-API-Key': API_KEY, 'Content-Type': 'application/json'}, method='POST')
    r2 = urllib.request.urlopen(req2, timeout=10)
    return r2.read().decode('utf-8', errors='replace')

print('=== Container MODEL_DIR ===')
print(run_cmd('echo $MODEL_DIR'))

# Download genre model
genre_url = 'https://essentia.upf.edu/models/classification-heads/genre_discogs400/genre_discogs400-discogs-effnet-1.pb'
genre_path = os.path.join(model_dir, 'discogs_400_epCNN_discogs-hard_256.pb')
if not os.path.exists(genre_path):
    print('Downloading genre model...')
    urllib.request.urlretrieve(genre_url, genre_path)
print(f'Genre model: {os.path.getsize(genre_path)} bytes')

# Deploy via Docker cp
print('Deploying to container...')
result = run_cmd(f'cp /dev/stdin /root/essentia_models/discogs_400_epCNN_discogs-hard_256.pb < {genre_path}')
print(result[:200])

# Alternative: use docker cp via exec
# Copy file to container using tar upload
tar_data = io.BytesIO()
with tarfile.open(fileobj=tar_data, mode='w') as tar:
    tar.add(genre_path, arcname='discogs_400_epCNN_discogs-hard_256.pb')
url = f'{API_URL}/api/endpoints/{EP}/docker/containers/{CONTAINER}/archive?path=/root/essentia_models/'
req = urllib.request.Request(url, data=tar_data.getvalue(), headers={'X-API-Key': API_KEY, 'Content-Type': 'application/x-tar'}, method='PUT')
try:
    r = urllib.request.urlopen(req, timeout=30)
    print(f'Archive upload: {r.status}')
except Exception as e:
    print(f'Archive upload failed: {e}')
    # Try writing directly via exec
    print('Trying exec copy...')
    import base64
    with open(genre_path, 'rb') as f:
        data = base64.b64encode(f.read()).decode()
    result = run_cmd(f'echo {data} | base64 -d > /root/essentia_models/discogs_400_epCNN_discogs-hard_256.pb')
    print(result[:200])

# Verify
print('\n=== Verify ===')
print(run_cmd('ls -la /root/essentia_models/'))

# Restart
url2 = f'{API_URL}/api/endpoints/{EP}/docker/containers/{CONTAINER}/restart'
req2 = urllib.request.Request(url2, data=b'{}', headers={'X-API-Key': API_KEY, 'Content-Type': 'application/json'}, method='POST')
r2 = urllib.request.urlopen(req2, timeout=30)
print(f'Essentia restarted: {r2.status}')
