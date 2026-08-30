# nd-organizer log webhook + dashboard.
#
# The Navidrome plugin cannot host a web page, so this small HTTP server is the
# receiver: the plugin POSTs its reports/status here and this server renders a
# clean, auto-refreshing dashboard. The integrations health panel is driven
# entirely by the plugin's status JSON (which contains the plugin's own checks
# of AcoustID/Lidarr/AudioMuse/MusicBrainz/Last.fm) - this server stores no
# API keys or URLs.
#
# Run standalone:
#   python server.py [port] [logfile]
# Or as a Docker service (see Dockerfile / docker-compose.yml).

import http.server
import json
import logging
import os
import socket
import sys
import time
import urllib.parse
import urllib.request
from datetime import datetime, timezone

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s %(levelname)s [webhook] %(message)s",
    datefmt="%Y-%m-%d %H:%M:%S",
)
log = logging.getLogger("webhook")

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 8099
LOGFILE = sys.argv[2] if len(sys.argv) > 2 else "webhook.log"
STARTED = time.time()

# Cap how many status events we keep in memory (oldest dropped). Older events
# stay in webhook.log; the dashboard shows the most recent MAX_ENTRIES.
MAX_ENTRIES = 2000
PLAYLIST_DIR = os.environ.get("PLAYLIST_DIR", "/data/playlists")
RADIO_DB_PATH = os.environ.get("NAVIDROME_DB", "/data/navidrome.db")
RADIO_BROWSER_API = os.environ.get("RADIO_BROWSER_API", "https://de1.api.radio-browser.info/json")

entries = []  # list of (ts, path, body)
services = {}  # sidecar name -> last heartbeat unix ts
last_any_request = time.time()  # webhook's own liveness
_playback_state = {}  # accumulated playback data across status posts

# Known sidecars and their HTTP ports, so the dashboard can pull each one's
# /logs by container name (they must share a Docker network with this webhook).
SIDECAR_LOG_PORTS = {
    "nd-organizer-acoustid": 8097,
    "nd-organizer-proxy": 4534,
    "nd-organizer-mysql": 8098,
    "nd-organizer-essentia": 8101,
}
_sidecar_logs = {}  # name -> (fetched_ts, text|None); refreshed every 30s
_sidecar_status = {}  # name -> (fetched_ts, dict|None)


# Per-render budget: the dashboard does synchronous sidecar probes, so we cap
# how long a single page render waits on them. Once the deadline passes, fetch
# helpers return cached/None so the page renders fast even with unreachable
# sidecars (they show as unavailable; the 5s auto-refresh catches them later).
_render_deadline = 0.0


def _within_budget():
    return _render_deadline == 0.0 or time.time() < _render_deadline


def _fetch_json(name, port, path, cache, ttl=30, timeout=1.0):
    now = time.time()
    # Use name+path as cache key so /health and /list don't collide.
    key = "%s%s" % (name, path)
    c = cache.get(key)
    if c and now - c[0] < ttl:
        return c[1]
    if not _within_budget():
        return c[1] if c else None
    try:
        req = urllib.request.Request(
            "http://%s:%d%s" % (name, port, path),
            headers={"Accept": "application/json"},
        )
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            val = json.loads(resp.read().decode("utf-8", "replace"))
        cache[key] = (time.time(), val)
        return val
    except Exception:
        cache[key] = (time.time(), None)
        return None


def _fetch_logs(name, port, timeout=1.0):
    now = time.time()
    c = _sidecar_logs.get(name)
    if c and now - c[0] < 30:
        return c[1]
    if not _within_budget():
        return c[1] if c else None
    try:
        req = urllib.request.Request(
            "http://%s:%d/logs" % (name, port),
            headers={"Accept": "text/plain"},
        )
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            text = resp.read().decode("utf-8", "replace").rstrip("\n")
        _sidecar_logs[name] = (time.time(), text)
        return text
    except Exception:
        _sidecar_logs[name] = (time.time(), None)
        return None


def _fmt_ts(ts):
    if not ts:
        return "never"
    try:
        return datetime.fromtimestamp(int(ts), tz=timezone.utc).strftime("%H:%M:%S")
    except Exception:
        return "?"


def _fhist_html(st):
    """Render a sidecar's recent filtered-track history as a list."""
    items = st.get("filtered") or []
    if not items:
        return "<div class='note'>Nothing filtered yet.</div>"
    rows = ""
    for it in items:
        if not isinstance(it, dict):
            continue
        reason = it.get("reason", "")
        chip = "<span class='chip k'>%s</span>" % esc(reason) if reason in ("keyword", "excluded") else ""
        rows += ("<div class='fh'><span class='ts'>%s</span><b>%s</b> "
                 "<span class='dim'>%s</span>%s</div>") % (
            _fmt_ts(it.get("ts")), esc(it.get("song", "") or it.get("id", "?")),
            esc(it.get("artist", "")), chip)
    return rows


def _sidecar_card(name, status, logs):
    """One rich card per sidecar. Unreachable sidecars show as 'OFFLINE' instead
    of being hidden, so the user sees all services at a glance."""
    short = name.replace("nd-organizer-", "")
    state, state_cls = "OK", "ok"
    if status is None and logs is None:
        state, state_cls = "OFFLINE", "bad"
    elif status:
        if status.get("inUse"):
            state, state_cls = "IN USE", "run"
        elif status.get("service") != name:
            state, state_cls = "unknown", "dim"
    stats = ""
    if status:
        s = status.get("stats") or {}
        if "stats" in status:  # acoustid
            stats = ("<div class='sc-stats'><span>lookups <b>%s</b></span>"
                     "<span>matches <b>%s</b></span><span>errors <b>%s</b></span>"
                     "<span>last match <b>%s</b></span><span>uptime <b>%s</b></span></div>") % (
                s.get("lookups", 0), s.get("matches", 0), s.get("errors", 0),
                _fmt_ts(s.get("lastMatch")), _uptime(status.get("uptime")))
        elif "requests" in status:  # proxy
            stats = ("<div class='sc-stats'><span>requests <b>%s</b></span>"
                     "<span>last <b>%s</b></span><span>skip-heavy <b>%s</b></span>"
                     "<span>weights <b>%s</b></span><span>keywords <b>%s</b></span>"
                     "<span>kw filter <b>%s</b></span><span>limit <b>%s</b></span></div>") % (
                status.get("requests", 0), _fmt_ts(status.get("lastRequest")),
                status.get("excluded", 0), status.get("weights", 0),
                len(status.get("keywords") or []),
                "on" if status.get("keywordFilter") else "off",
                esc(status.get("skipMode", "none")))
        elif "ops" in status:  # mysql
            db = status.get("db")
            if db:
                stats = ("<div class='sc-stats'><span>ops <b>%s</b></span>"
                         "<span>last op <b>%s</b></span><span>rows <b>%s</b></span>"
                         "<span>size <b>%s</b></span><span>last update <b>%s</b></span></div>") % (
                    status.get("ops", 0), esc(status.get("lastOp", "")),
                    db.get("rows", "?"), _fmt_bytes(db.get("bytes", 0)),
                    esc(db.get("lastUpdate") or "never"))
            else:
                stats = ("<div class='sc-stats'><span>ops <b>%s</b></span>"
                         "<span>last op <b>%s</b></span>"
                         "<span class='dim'>db not used yet</span></div>") % (
                    status.get("ops", 0), esc(status.get("lastOp", "")))
    extra = ""
    if status and "filtered" in status:
        extra = ("<details><summary>recently filtered (%d)</summary><div class='fhist'>%s</div></details>"
                 % (len(status.get("filtered") or []), _fhist_html(status)))
    logs_html = ""
    if logs:
        lines = logs.splitlines()
        logs_html = ("<details><summary>logs <span class='dim'>%d lines</span></summary><pre>%s</pre></details>"
                     % (len(lines), esc(logs)))
    return ("<div class='sc'><div class='sc-top'><b>%s</b>"
            "<span class='tag %s'>%s</span></div>%s%s%s</div>") % (
        esc(name), state_cls, state, stats, extra, logs_html)


def _uptime(secs):
    try:
        secs = int(secs or 0)
    except (TypeError, ValueError):
        return "?"
    return "%dh %dm" % (secs // 3600, (secs % 3600) // 60)


def _fmt_bytes(n):
    try:
        n = int(n or 0)
    except (TypeError, ValueError):
        return "?"
    for unit in ("B", "KB", "MB", "GB"):
        if n < 1024 or unit == "GB":
            return "%d %s" % (n, unit)
        n //= 1024
    return "?"


def sidecar_logs_html():
    """Fetch each sidecar's /health + /logs (cached 30s) and render rich cards
    so this dashboard is the single UI for the whole project. Unreachable
    sidecars show as OFFLINE. MySQL card is hidden when persistenceBackend != mysql."""
    # Check if MySQL is actually in use by looking at the latest status
    mysql_in_use = False
    for _, _, body in reversed(entries):
        try:
            j = json.loads(body)
            if isinstance(j, dict) and j.get("persistenceBackend") == "mysql":
                mysql_in_use = True
                break
        except Exception:
            continue
    out = []
    for name, port in sorted(SIDECAR_LOG_PORTS.items()):
        # Hide MySQL card entirely when not configured
        if name == "nd-organizer-mysql" and not mysql_in_use:
            continue
        card = _sidecar_card(name, _fetch_json(name, port, "/health", _sidecar_status), _fetch_logs(name, port))
        if card:
            out.append(card)
    octo = _octo_fiesta_card()
    if octo:
        out.append(octo)
    # Webhook's own card with recent log entries
    try:
        log_lines = read_tail(LOGFILE, 20)
        if log_lines:
            logs_html = "<div class='fhist'>"
            for line in log_lines[-15:]:
                logs_html += "<div class='fh'><span class='dim'>%s</span></div>" % esc(line.rstrip()[:120])
            logs_html += "</div>"
        else:
            logs_html = "<div class='note'>No log entries yet.</div>"
        out.append(("<div class='sc'><div class='sc-top'><b>nd-organizer-webhook</b>"
                     "<span class='tag ok'>OK</span></div>"
                     "<div class='sc-stats'><span>events <b>%d</b></span>"
                     "<span>log <b>%s</b></span></div>"
                     "<details><summary>recent logs</summary>%s</details></div>") % (
            len(entries), esc(LOGFILE), logs_html))
    except Exception:
        pass
    if not out:
        return "<div class='note'>No sidecar is running.</div>"
    return "".join(out)


# ---------------------------------------------------------------- internet radio
#
# Radio panel driven by the radio management (built into webhook) (WB2024/Add-Navidrome-
# Radios): search/add internet radio stations straight into Navidrome's `radio`
# table. Hidden when the sidecar is unreachable.

def radio_html():
    """Internet radio panel: existing stations + AJAX search/add/remove/rename.
    All operations use local functions (radio sidecar merged into webhook)."""
    if not radio_table_exists():
        return ("<div class='card now'><h2>Internet radio</h2>"
                "<div class='note'>Navidrome radio table not found — mount navidrome.db at %s.</div></div>") % RADIO_DB_PATH
    out = "<div class='card now'><h2>Internet radio</h2>"
    # --- Station list with Remove + Rename ---
    stations = radio_list_stations()
    rows = ""
    for s in stations[:50]:
        n = esc(s.get("name", "?"))
        u = esc(s.get("url", ""))
        name_json = json.dumps(s.get("name", "")).replace('"', '&quot;')
        url_json = json.dumps(s.get("url", "")).replace('"', '&quot;')
        rows += ("<div class='fh'><b>%s</b> <span class='dim'>%s</span>"
                 " <button class='radio-rm' onclick='radioRemove(%s,%s)'>Remove</button>"
                 " <button class='radio-rn' onclick='radioRename(%s)'>Rename</button>"
                 "</div>") % (n, u, name_json, url_json, name_json)
    out += "<div class='sc-stats'><span>stations <b>%d</b></span></div>" % len(stations)
    out += "<div class='np-head'>Stations</div><div id='radio-stations'>"
    if rows:
        out += rows
    else:
        out += "<div class='note'>None yet — search below.</div>"
    out += "</div>"
    # --- Search form (AJAX) ---
    out += ("<form class='radio-search' onsubmit='return radioSearch(event)'>"
            "<input type='text' id='radioQ' placeholder='Search name / genre / country' autocomplete='off'>"
            "<select id='radioType'><option value='byname'>Name</option>"
            "<option value='bytag'>Genre</option>"
            "<option value='bycountry'>Country</option></select>"
            "<button type='submit'>Search</button></form>"
            "<div id='radioResults'></div>")
    # --- Inline JavaScript ---
    out += ("<script>"
            "function esc(s){return s.replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/\"/g,'&quot;')}"
            "function radioSearch(e){"
            "  e.preventDefault();"
            "  var q=document.getElementById('radioQ').value.trim();"
            "  var t=document.getElementById('radioType').value;"
            "  if(!q){return false}"
            "  var el=document.getElementById('radioResults');"
            "  el.innerHTML='<div class=\"note\">Searching...</div>';"
            "  fetch('/radio-lookup?q='+encodeURIComponent(q)+'&type='+encodeURIComponent(t))"
            "    .then(function(r){return r.json()})"
            "    .then(function(d){"
            "      var results=d.results||[];"
            "      if(!results.length){el.innerHTML='<div class=\"note\">No results for \"'+esc(q)+'\"</div>';return}"
            "      var h='<div class=\"np-head\">Results</div>';"
            "      results.slice(0,20).forEach(function(s){"
            "        var nn=JSON.stringify(s.name);var u=JSON.stringify(s.url);var hp=JSON.stringify(s.homepage||'');"
            "        h+='<div class=\"fh\"><b>'+esc(s.name||'')+'</b> <span class=\"dim\">'+esc((s.tags||'').substring(0,35))+'</span>'"
            "          +'<button class=\"radio-add\" onclick=\"radioAdd(this,'+nn+','+u+','+hp+')\">Add</button></div>';"
            "      });"
            "      el.innerHTML=h;"
            "    }).catch(function(){el.innerHTML='<div class=\"note\">Search failed.</div>'});"
            "  return false;"
            "}"
            "function radioAdd(btn,name,url,hp){"
            "  var orig=btn.textContent;btn.disabled=true;btn.innerHTML='<span class=\"spin\"></span> Adding...';"
            "  fetch('/radio-add',{method:'POST',headers:{'Content-Type':'application/json'},"
            "    body:JSON.stringify({stations:[{name:name,url:url,homepage:hp||''}]})"
            "  }).then(function(r){return r.json()}).then(function(d){"
            "    if(!d.ok && d.error) alert(d.error);"
            "    radioRefreshList();"
            "  }).catch(function(e){alert('Add failed: '+e);btn.disabled=false;btn.textContent=orig;});"
            "}"
            "function radioRemove(name,url){"
            "  if(!confirm('Remove '+name+'?'))return;"
            "  fetch('/radio-remove',{method:'POST',headers:{'Content-Type':'application/json'},"
            "    body:JSON.stringify({name:name,url:url})"
            "  }).then(function(){radioRefreshList()});"
            "}"
            "function radioRename(oldName){"
            "  var nn=prompt('Rename station:',oldName);"
            "  if(!nn||nn===oldName)return;"
            "  fetch('/radio-rename',{method:'POST',headers:{'Content-Type':'application/json'},"
            "    body:JSON.stringify({old_name:oldName,new_name:nn})"
            "  }).then(function(){radioRefreshList()});"
            "}"
            "function radioRefreshList(){"
            "  fetch('/radio-list',{headers:{'Accept':'application/json'}})"
            "  .then(function(r){return r.json()}).then(function(d){"
            "    var el=document.getElementById('radio-stations');"
            "    if(!el)return;"
            "    var st=d.stations||[];"
            "    if(!st.length){el.innerHTML='<div class=\"note\">No stations configured.</div>';return;}"
            "    var h='<table><tr><th>Name</th><th>URL</th><th></th></tr>';"
            "    st.forEach(function(s){"
            "      var nj=JSON.stringify(s.name);var uj=JSON.stringify(s.url);"
            "      h+='<tr><td>'+esc(s.name)+'</td><td class=\"dim\">'+esc(s.url)+'</td>"
            "      <td><button onclick=\\'radioRemove('+nj+','+uj+')\\'>x</button> "
            "      <button onclick=\\'radioRename('+nj+')\\'>rename</button></td></tr>';"
            "    });"
            "    h+='</table>';"
            "    el.innerHTML=h;"
            "  }).catch(function(){});"
            "}"
            "</script>")
    out += "</div>"
    return out


# ---------------------------------------------------------------- smart playlists
#
# Build and manage Navidrome .nsp smart playlists directly from the dashboard.
# No third-party tools required — the JSON rule format is generated server-side
# and saved to the configured playlist directory.

SMART_PRESETS = [
    {"name": "Recently Played", "comment": "Tracks played in the last 30 days",
     "all": [{"inTheLast": {"lastplayed": 30}}], "sort": "-lastplayed"},
    {"name": "Most Played", "comment": "Top 100 most-played tracks",
     "all": [{"gt": {"playcount": 10}}], "sort": "-playcount", "limit": 100},
    {"name": "Loved Tracks", "comment": "All favourited tracks",
     "all": [{"is": {"loved": True}}], "sort": "-dateadded"},
    {"name": "Top Rated", "comment": "Tracks rated 4+ stars",
     "all": [{"gt": {"rating": 4}}], "sort": "-rating"},
    {"name": "Never Played", "comment": "Tracks you haven't heard yet",
     "all": [{"or": [{"is": {"playcount": 0}}, {"is": {"lastplayed": 0}}]}], "sort": "random"},
    {"name": "Recently Added", "comment": "Newest additions to your library",
     "all": [{"inTheLast": {"dateadded": 30}}], "sort": "-dateadded"},
    {"name": "FLAC Only", "comment": "Lossless files only",
     "all": [{"is": {"filetype": "flac"}}], "sort": "random"},
    {"name": "High Energy", "comment": "BPM above 140",
     "all": [{"gt": {"bpm": 140}}], "sort": "-bpm"},
    {"name": "Chill", "comment": "BPM under 100",
     "all": [{"lt": {"bpm": 100}}], "sort": "bpm", "limit": 50},
    {"name": "Favourite Albums", "comment": "Loved tracks from albums with 3+ loved tracks",
     "all": [{"is": {"loved": True}}, {"gt": {"rating": 3}}], "sort": "-rating"},
    {"name": "Short and Sweet", "comment": "Tracks under 3 minutes",
     "all": [{"lt": {"duration": 180}}], "sort": "random"},
    {"name": "Epic Tracks", "comment": "Tracks over 6 minutes",
     "all": [{"gt": {"duration": 360}}], "sort": "-duration"},
    {"name": "Classics", "comment": "Released before 1990, still loved",
     "all": [{"lt": {"year": 1990}}, {"is": {"loved": True}}], "sort": "-year"},
    {"name": "Recent Favourites", "comment": "Loved tracks added in the last 90 days",
     "all": [{"is": {"loved": True}}, {"inTheLast": {"dateadded": 90}}], "sort": "-dateadded"},
    {"name": "Deep Cuts", "comment": "Unplayed album tracks (track 5+)",
     "all": [{"gt": {"track": 4}}, {"is": {"playcount": 0}}], "sort": "+year,-track"},
]


def playlist_dir():
    """Ensure the playlist directory exists and return it."""
    d = PLAYLIST_DIR
    os.makedirs(d, exist_ok=True)
    return d


def list_playlists():
    """List all .nsp files in the playlist directory."""
    d = playlist_dir()
    if not os.path.isdir(d):
        return []
    out = []
    for f in sorted(os.listdir(d)):
        if f.endswith(".nsp"):
            try:
                with open(os.path.join(d, f), "r", encoding="utf-8") as fh:
                    data = json.load(fh)
                out.append({"file": f, "name": data.get("name", f), "comment": data.get("comment", "")})
            except Exception:
                out.append({"file": f, "name": f, "comment": "(invalid)"})
    return out


def save_playlist(name, comment, nsp_body):
    """Save a smart playlist as a .nsp JSON file. Returns (ok, filename, error)."""
    d = playlist_dir()
    safe_name = "".join(c if c.isalnum() or c in "-_ " else "_" for c in name).strip()[:60]
    if not safe_name:
        return False, "", "invalid playlist name"
    filename = f"{safe_name}.nsp"
    path = os.path.join(d, filename)
    body = {"name": name, "comment": comment}
    body.update(nsp_body)
    try:
        with open(path, "w", encoding="utf-8") as fh:
            json.dump(body, fh, indent=2, ensure_ascii=False)
        log.info("playlist saved: %s (%d rules)", filename, len(nsp_body.get("all", nsp_body.get("any", []))))
        return True, filename, ""
    except Exception as e:
        return False, "", str(e)


def delete_playlist(filename):
    """Delete a .nsp file. Returns (ok, error)."""
    if not filename.endswith(".nsp"):
        return False, "must be a .nsp file"
    path = os.path.join(playlist_dir(), filename)
    try:
        os.remove(path)
        log.info("playlist deleted: %s", filename)
        return True, ""
    except Exception as e:
        return False, str(e)


def playlist_html():
    """Smart Playlist panel: list existing + create new / deploy presets."""
    out = "<div class='card now'><h2>Smart Playlists</h2>"
    pl = list_playlists()
    rows = ""
    for p in pl[:30]:
        nm = esc(p.get("name", "?"))
        cm = esc(p.get("comment", "")[:40])
        fn = esc(p.get("file", ""))
        rows += ("<div class='fh'><b>%s</b> <span class='dim'>%s</span>"
                 "<button class='radio-rm' onclick=\"playlistDelete('%s')\">Remove</button></div>") % (nm, cm, fn)
    out += "<div class='sc-stats'><span>playlists <b>%d</b></span></div>" % len(pl)
    out += "<div class='np-head'>Saved Playlists</div><div id='playlist-list'>"
    if rows:
        out += rows
    else:
        out += "<div class='note'>None yet — create one or deploy a preset.</div>"
    out += "</div>"
    # Preset deploy
    out += ("<div class='np-head'>Presets</div>"
            "<select id='presetSelect' class='radio-search'>"
            "<option value=''>Choose a preset...</option>")
    for p in SMART_PRESETS:
        out += "<option value='%s'>%s — %s</option>" % (esc(p['name']), esc(p['name']), esc(p['comment']))
    out += "</select><button class='radio-rm' onclick='deployPreset()'>Deploy</button>"
    # Create new (simple rule builder)
    out += ("<div class='np-head'>Create New</div>"
            "<form class='radio-search' onsubmit='return createPlaylist(event)'>"
            "<input id='plName' placeholder='Playlist name' style='width:180px'>"
            "<input id='plComment' placeholder='Description (optional)' style='width:180px'>"
            "<select id='plField'><option value='loved'>Loved</option>"
            "<option value='rating'>Rating</option><option value='playcount'>Play Count</option>"
            "<option value='year'>Year</option><option value='genre'>Genre</option>"
            "<option value='bpm'>BPM</option><option value='duration'>Duration (s)</option>"
            "<option value='albumartist'>Artist</option><option value='album'>Album</option>"
            "<option value='filetype'>File Type</option></select>"
            "<select id='plOp'><option value='gt'>>=</option>"
            "<option value='lt'>&lt;=</option><option value='is'>equals</option>"
            "<option value='contains'>contains</option></select>"
            "<input id='plVal' placeholder='value' style='width:120px'>"
            "<button type='submit'>Save</button></form>")
    out += ("<script>"
            "function playlistSave(name,comment,body){"
            "  fetch('/playlist-save',{method:'POST',headers:{'Content-Type':'application/json'},"
            "    body:JSON.stringify({name:name,comment:comment,nsp:body})"
            "  }).then(function(r){return r.json()})"
            "  .then(function(d){if(!d.ok)alert(d.error||'Save failed');playlistRefreshList();})"
            "  .catch(function(){alert('Save failed');});"
            "}"
            "function playlistDelete(file){"
            "  if(!confirm('Delete '+file+'?'))return;"
            "  fetch('/playlist-delete',{method:'POST',headers:{'Content-Type':'application/json'},"
            "    body:JSON.stringify({file:file})"
            "  }).then(function(r){return r.json()})"
            "  .then(function(d){if(!d.ok)alert(d.error||'Delete failed');playlistRefreshList();})"
            "  .catch(function(){alert('Delete failed');});"
            "}"
            "function playlistRefreshList(){"
            "  fetch('/playlist-list').then(function(r){return r.json()})"
            "  .then(function(d){"
            "    var el=document.getElementById('playlist-list');"
            "    if(!el)return;"
            "    var pls=d.playlists||[];"
            "    if(!pls.length){el.innerHTML='<div class=\"note\">No saved playlists.</div>';return;}"
            "    var h='<table><tr><th>Name</th><th>Rules</th><th></th></tr>';"
            "    pls.forEach(function(p){"
            "      var fn=JSON.stringify(p.filename||p.name);"
            "      h+='<tr><td>'+esc(p.name||p.filename)+'</td><td class=\"dim\">'+esc((p.comment||'').substring(0,40))+'</td>"
            "      <td><button onclick=\\'playlistDelete('+fn+')\\'>x</button></td></tr>';"
            "    });"
            "    h+='</table>';"
            "    el.innerHTML=h;"
            "  }).catch(function(){});"
            "}"
            "function deployPreset(){"
            "  var s=document.getElementById('presetSelect').value;"
            "  if(!s)return;"
            "  fetch('/playlist-presets?action=deploy&name='+encodeURIComponent(s))"
            "    .then(function(r){return r.json()})"
            "    .then(function(d){alert(d.ok?'Deployed: '+d.filename:d.error);playlistRefreshList()})"
            "    .catch(function(){alert('Deploy failed')});"
            "}"
            "function createPlaylist(e){"
            "  e.preventDefault();"
            "  var n=document.getElementById('plName').value.trim();"
            "  var c=document.getElementById('plComment').value.trim();"
            "  var field=document.getElementById('plField').value;"
            "  var op=document.getElementById('plOp').value;"
            "  var val=document.getElementById('plVal').value.trim();"
            "  if(!n||!field||!op||!val){alert('Fill all fields');return}"
            "  var rule={}; rule[field]={}; rule[field][op]=val;"
            "  var nsp={name:n,comment:c,all:[rule]};"
            "  playlistSave(n,c,nsp);"
            "}"
            "</script>")
    out += "</div>"
    return out


# ---------------------------------------------------------------- internet radio
#
# Internet radio management built into the webhook (no separate sidecar needed).
# Uses Navidrome's SQLite radio table directly. Search via Radio-Browser API.

def radio_db_connect():
    import sqlite3
    conn = sqlite3.connect(RADIO_DB_PATH, timeout=10)
    conn.row_factory = sqlite3.Row
    return conn

def radio_table_exists():
    try:
        conn = radio_db_connect()
        cur = conn.cursor()
        cur.execute("SELECT name FROM sqlite_master WHERE type='table' AND name='radio'")
        ok = cur.fetchone() is not None
        conn.close()
        return ok
    except Exception:
        return False

def radio_rb_get(path):
    import urllib.request as _ur
    url = RADIO_BROWSER_API.rstrip("/") + path
    req = _ur.Request(url, headers={"User-Agent": "nd-organizer-webhook/1.0", "Accept": "application/json"})
    with _ur.urlopen(req, timeout=15) as r:
        return json.loads(r.read().decode("utf-8", "replace"))

def radio_search(query, search_type="byname", limit=30):
    if search_type == "top":
        return radio_rb_get(f"/stations/topvote/{max(1, limit)}")
    q = urllib.parse.quote(query)
    return radio_rb_get(f"/stations/{search_type}/{q}?limit={max(1, limit)}&hidebroken=true")

def radio_station_exists(cur, name, url):
    cur.execute("SELECT id FROM radio WHERE name = ? OR stream_url = ?", (name, url))
    return cur.fetchone() is not None

def radio_add_stations(stations):
    import base64, hashlib, datetime
    added = 0
    skipped = 0
    errors = []
    try:
        conn = radio_db_connect()
        cur = conn.cursor()
        for st in stations:
            name = (st.get("name") or "").strip()
            url = (st.get("url") or st.get("stream_url") or "").strip()
            if not name or not url:
                continue
            if radio_station_exists(cur, name, url):
                skipped += 1
                continue
            unique = f"{name}{datetime.datetime.utcnow().isoformat()}"
            station_id = base64.b64encode(hashlib.md5(unique.encode()).digest()).decode().rstrip("=").replace("+", "-").replace("/", "_")[:22]
            ts = datetime.datetime.utcnow().strftime("%Y-%m-%d %H:%M:%S.%f")
            homepage = (st.get("homepage") or "").strip()
            cur.execute(
                "INSERT INTO radio (id, name, stream_url, home_page_url, created_at, updated_at) "
                "VALUES (?, ?, ?, ?, ?, ?)",
                (station_id, name, url, homepage, ts, ts),
            )
            added += 1
        conn.commit()
        conn.close()
    except Exception as e:
        errors.append(str(e))
    return added, skipped, errors

def radio_list_stations():
    try:
        conn = radio_db_connect()
        cur = conn.cursor()
        cur.execute("SELECT name, stream_url FROM radio ORDER BY name")
        rows = [{"name": r["name"], "url": r["stream_url"]} for r in cur.fetchall()]
        conn.close()
        return rows
    except Exception:
        return []


# ---------------------------------------------------------------- octo-fiesta
#
# Octo-Fiesta is a third-party Subsonic proxy - it exposes only the Subsonic
# API (no /status, no /logs). Its URL + provider come from the plugin's status
# POST (Navidrome plugin config, like the other sidecar URLs); health is a
# Subsonic ping; activity ("intercept") is its Docker logs, read through the
# mounted (read-only) Docker socket.

OCTO_FIESTA_CONTAINER = os.environ.get("OCTO_FIESTA_CONTAINER", "octo-fiesta")
DOCKER_SOCK = os.environ.get("DOCKER_SOCK", "/var/run/docker.sock")
_octo_health = {}  # -> (ts, ok, detail)
_octo_logs = {}    # -> (ts, text|None)


def _octo_fiesta_config():
    """Latest octoFiestaUrl / octoFiestaProvider from the plugin status POST."""
    url, provider = "", "SquidWTF"
    for _, _, body in reversed(entries):
        try:
            j = json.loads(body)
        except Exception:
            continue
        if isinstance(j, dict):
            if j.get("octoFiestaUrl"):
                url = str(j["octoFiestaUrl"]).strip()
            if j.get("octoFiestaProvider"):
                provider = str(j["octoFiestaProvider"]).strip()
            if url:
                break
    return url.rstrip("/"), provider or "SquidWTF"


def _octo_fiesta_health():
    now = time.time()
    c = _octo_health.get("v")
    if c and now - c[0] < 30:
        return c[1], c[2]
    url, _ = _octo_fiesta_config()
    if not url:
        _octo_health["v"] = (time.time(), False, "not configured")
        return False, "not configured"
    if not _within_budget():
        return c[1], c[2]
    try:
        req = urllib.request.Request(
            url + "/rest/ping", headers={"Accept": "text/xml"})
        with urllib.request.urlopen(req, timeout=1.5) as resp:
            body = resp.read().decode("utf-8", "replace")
            ok = resp.status == 200 and 'status="ok"' in body
            _octo_health["v"] = (time.time(), ok, "HTTP %d" % resp.status)
            return ok, "HTTP %d" % resp.status
    except Exception as e:
        _octo_health["v"] = (time.time(), False, str(e)[:80])
        return False, str(e)[:80]


def _docker_logs(container, tail=300):
    """Read a container's recent stdout/stderr via the Docker Engine API over
    the unix socket. Returns None when the socket isn't mounted/usable. Capped
    at ~2 MiB so a chatty container can't stall the dashboard render."""
    if not os.path.exists(DOCKER_SOCK):
        return None
    if not _within_budget():
        return None
    try:
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        sock.settimeout(4)
        sock.connect(DOCKER_SOCK)
        path = "/containers/%s/logs?stdout=1&stderr=1&tail=%d&timestamps=0" % (container, tail)
        sock.sendall(("GET %s HTTP/1.1\r\nHost: docker\r\nConnection: close\r\n\r\n" % path).encode())
        data = b""
        while len(data) < 2 * 1024 * 1024:
            chunk = sock.recv(65536)
            if not chunk:
                break
            data += chunk
        sock.close()
        if not data:
            return None
        _, _, body = data.partition(b"\r\n\r\n")
        # Docker log stream: 8-byte frame header (stream byte + 4-byte length).
        out, i, n = [], 0, len(body)
        while i + 8 <= n:
            size = int.from_bytes(body[i + 4:i + 8], "big")
            out.append(body[i + 8:i + 8 + size].decode("utf-8", "replace"))
            i += 8 + size
        return "".join(out).rstrip("\n")
    except Exception:
        return None


def _octo_fiesta_logs():
    now = time.time()
    c = _octo_logs.get("v")
    if c and now - c[0] < 30:
        return c[1]
    text = _docker_logs(OCTO_FIESTA_CONTAINER)
    _octo_logs["v"] = (time.time(), text)
    return text


def _octo_fiesta_card():
    """Health + recent activity card for octo-fiesta. Shown whenever the plugin
    reports an octoFiestaUrl: ONLINE with stats+logs when reachable, an
    UNREACHABLE health pill (with the failure detail) when it can't be pinged.
    Hidden only when octo isn't configured."""
    url, provider = _octo_fiesta_config()
    if not url:
        return ""  # not configured
    ok, detail = _octo_fiesta_health()
    state, state_cls = ("ONLINE", "ok") if ok else ("UNREACHABLE", "bad")
    rows = ""
    logs = _octo_fiesta_logs()
    if logs:
        keep = []
        for line in logs.splitlines():
            low = line.lower()
            if any(k in low for k in ("download", "fetched", "provider", "squid",
                                      "deezer", "qobuz", "yandex", "error",
                                      "stream", "external", "missing", "fail")):
                keep.append(line)
        rows = "".join("<div class='fh'><span class='dim'>%s</span></div>" % esc(l)
                       for l in keep[-25:])
        if not keep:
            rows = "<div class='note'>octo-fiesta is online; no provider activity logged recently.</div>"
    else:
        rows = "<div class='note'>Logs unavailable (Docker socket not mounted).</div>"
    return ("<div class='card'><h2>Octo-Fiesta <span class='tag mode'>missing-track proxy</span></h2>"
            "<div class='now-top'><span class='pill %s'>%s</span>"
            "<span class='now-line'>ping %s &middot; <a href='%s'>open</a></span></div>"
            "<div class='sc-stats'><span>health <b>%s</b></span>"
            "<span>provider <b>%s</b></span><span>container <b>%s</b></span></div>"
            "%s</div>") % (
        state_cls, state, esc(detail), esc(url),
        esc(detail), esc(provider),
        esc(OCTO_FIESTA_CONTAINER), rows)


def load_log():
    """Load only the most recent events from the log file, then self-clean it.

    Reading the whole file (500k+ lines) every startup is what stalled the
    dashboard. We read only the TAIL (newest MAX_ENTRIES lines) so startup is
    instant no matter how large the log has grown, then truncate the file so it
    never balloons out of control again.
    """
    try:
        tail = read_tail(LOGFILE, MAX_ENTRIES)
        for line in tail:
            line = line.rstrip("\n")
            if line.startswith("[") and "] " in line:
                ts, rest = line[1:].split("] ", 1)
                path, body = rest.split(" - ", 1)
                entries.append((ts, path, body))
        if len(entries) > MAX_ENTRIES:
            del entries[: len(entries) - MAX_ENTRIES]
        _self_clean_log(LOGFILE)
    except FileNotFoundError:
        pass


def read_tail(path, n):
    """Return the last `n` non-empty lines of a file without loading it all.
    Reads backwards in chunks so a huge file is O(file tail), not O(whole file)."""
    lines = []
    try:
        with open(path, "rb") as f:
            f.seek(0, os.SEEK_END)
            size = f.tell()
            block = 8192
            buf = b""
            pos = size
            while pos > 0 and len(lines) < n:
                read_len = min(block, pos)
                pos -= read_len
                f.seek(pos)
                chunk = f.read(read_len)
                buf = chunk + buf
                # Split on newlines; keep the newest complete lines.
                parts = buf.split(b"\n")
                buf = parts[0]  # partial line at the front (older)
                for p in reversed(parts[1:]):
                    if p.strip():
                        lines.append(p.decode("utf-8", "replace"))
                        if len(lines) >= n:
                            break
                if len(lines) >= n:
                    break
            # If we consumed the whole file and buf still has a partial line.
            if buf.strip() and len(lines) < n:
                lines.append(buf.decode("utf-8", "replace"))
        lines.reverse()
        return lines
    except FileNotFoundError:
        return []
    except OSError:
        return []


def _self_clean_log(path):
    """Keep the log file bounded: if it's grown large, rewrite it to only the
    newest MAX_ENTRIES lines so a stale backlog never returns. Best-effort."""
    try:
        size = os.path.getsize(path)
        # Small files are left alone; only trim once the file is well over the
        # in-memory cap (rough heuristic, avoids rewriting constantly).
        if size < MAX_ENTRIES * 256:
            return
        keep = read_tail(path, MAX_ENTRIES)
        with open(path, "w", encoding="utf-8") as f:
            f.write("\n".join(keep) + ("\n" if keep else ""))
        log.info("self-clean: trimmed webhook.log to %d lines", len(keep))
    except Exception:
        pass


def esc(s):
    return (str(s).replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;"))


# ---------------------------------------------------------------- integrations

def integrations_html():
    """Render the integrations panel. Combines plugin-reported status with
    webhook-probed external API health (MusicBrainz, ListenBrainz)."""
    found = None
    ts = None
    for _, _, body in reversed(entries):
        try:
            j = json.loads(body)
        except Exception:
            continue
        if isinstance(j, dict) and isinstance(j.get("integrations"), list):
            found = j["integrations"]
            ts = j.get("ts")
            break
    if found is None:
        return ("<div class='note'>Waiting for the plugin to report integration status "
                "(enable favorites/status checks in Navidrome plugin settings).</div>")

    state_cls = {"ok": "ok", "reachable": "warn", "unreachable": "bad",
                 "authFailed": "authfail", "notConfigured": "dim"}
    state_label = {"ok": "OK", "reachable": "PARTIAL", "unreachable": "UNREACHABLE",
                   "authFailed": "AUTH FAILED", "notConfigured": "NOT CONFIGURED"}

    healthy = warn = bad = notc = 0
    issues = []
    cards = ""
    plugin_names = set()
    for it in found:
        if not isinstance(it, dict):
            continue
        name = it.get("name", "?")
        plugin_names.add(name.lower())
        state = it.get("state", "unknown")
        detail = it.get("detail", "")
        cls = state_cls.get(state, "dim")
        label = state_label.get(state, state.upper())
        if state == "ok":
            healthy += 1
        elif state == "reachable":
            warn += 1
        elif state in ("unreachable", "authFailed"):
            bad += 1
            issues.append("%s - %s" % (name, label))
        else:
            notc += 1
        cards += ("<div class='ig'><div class='ig-top'><span class='ig-name'>%s</span>"
                  "<span class='ig-state %s'>%s</span></div>%s</div>") % (
            esc(name), cls, label,
            "<span class='dim'>%s</span>" % esc(detail) if detail else "")

    total = healthy + warn + bad + notc
    checked = ""
    if ts:
        try:
            dt = datetime.fromtimestamp(int(ts), tz=timezone.utc)
            age_min = max(0, int((datetime.now(tz=timezone.utc) - dt).total_seconds() // 60))
            checked = "checked %s UTC" % dt.strftime("%H:%M:%S")
            if age_min > 15:
                checked += " | STALE (%dm ago)" % age_min
            else:
                checked += " | %dm ago" % age_min
        except Exception:
            pass

    if bad:
        banner = ("<div class='alert bad'><b>%d service%s need attention:</b> %s</div>"
                  % (bad, "" if bad == 1 else "s", esc(" | ".join(issues))))
    elif warn:
        banner = ("<div class='alert warn'><b>%d service%s at reduced capacity.</b></div>"
                  % (warn, "" if warn == 1 else "s"))
    else:
        banner = ""

    if total:
        summary = ("<div class='ig-sum'><b>%d/%d</b> healthy"
                   % (healthy + warn, total))
        if bad or warn:
            summary += " &middot; <span class='%s'>%d need attention</span>" % (
                "ig-state bad" if bad else "ig-state warn", bad + warn)
        summary += (" %s</div>" % esc(checked) if checked else "</div>")
    else:
        summary = ""

    # External APIs (MusicBrainz, ListenBrainz) are probed only when the
    # Docker network has outbound internet access. Skip if not reachable.
    # The plugin handles their actual functionality; health cards are optional.

    cards += service_cards(skip=plugin_names)

    return banner + summary + "<div class='integrations'>%s</div>" % cards

def service_cards(skip=None):
    """Sidecar liveness cards (from heartbeats + the webhook's own last
    request). Green = seen recently, red = stale/no signal. Services already
    reported by the plugin's integration checks (same endpoint) are skipped so
    they aren't shown twice, and sidecars that haven't signalled in a while are
    hidden entirely (they're not running)."""
    skip = skip or set()
    now = time.time()
    services["webhook"] = last_any_request
    display = {"acoustid": "AcoustID", "proxy": "Proxy", "webhook": "Webhook"}
    cards = ""
    for name in sorted(services):
        if name.lower() in skip:
            continue
        if name.lower() == "mysql":
            continue  # MySQL is handled separately in sidecar_logs_html
        age = max(0, int(now - services[name]))
        if age > 120:
            continue  # no signal in 2 min -> not running, hide it
        label_name = display.get(name.lower(), name.title())
        if age < 120:
            cls, label = "ok", "UP"
        else:
            cls, label = "warn", "WEAK"
        cards += ("<div class='ig'><div class='ig-top'><span class='ig-name'>%s</span>"
                  "<span class='ig-state %s'>%s</span></div>"
                  "<span class='dim'>last signal %ds ago</span></div>") % (
            esc(label_name), cls, label, age)
    return cards


def tasks_html():
    """Render the plugin's recent task queue (scan chunks, plan batches,
    favsync, stats) from the latest status JSON so the user sees what is
    processing and what happened."""
    found = None
    for _, _, body in reversed(entries):
        try:
            j = json.loads(body)
        except Exception:
            continue
        if isinstance(j, dict) and isinstance(j.get("tasks"), list) and j["tasks"]:
            found = j["tasks"]
            break
    if not found:
        return "<div class='note'>No task activity yet - task progress appears here as it runs.</div>"
    state_cls = {"running": "run", "done": "ok", "failed": "bad"}
    rows = ""
    n_run = n_done = n_fail = 0
    for t in found:
        if not isinstance(t, dict):
            continue
        st = t.get("state", "?")
        if st == "running":
            n_run += 1
        elif st == "done":
            n_done += 1
        elif st == "failed":
            n_fail += 1
        cls = state_cls.get(st, "dim")
        ts = ""
        if t.get("ts"):
            try:
                ts = datetime.fromtimestamp(int(t["ts"]), tz=timezone.utc).strftime("%H:%M:%S")
            except Exception:
                pass
        lib = t.get("libraryId")
        rows += ("<div class='tk'><span class='tk-ts'>%s</span>"
                 "<span class='tag %s'>%s</span>"
                 "<span class='tk-kind'>%s</span>%s"
                 "<span class='tk-msg'>%s</span></div>") % (
            esc(ts), cls, esc(st.upper()),
            esc(t.get("kind", "?")),
            "<span class='dim'>&middot; lib %s</span>" % esc(str(lib)) if lib else "",
            esc(t.get("message", "")))
    total = n_run + n_done + n_fail
    pct = int((n_done * 100.0 / total)) if total else 0
    bar = ("<div class='kv'><span class='tag ok'>done %d</span>"
           "<span class='tag run'>running %d</span>"
           "<span class='tag bad'>failed %d</span></div>"
           "<div class='bar f'><i style='width:%d%%;background:linear-gradient(90deg,#0f3d24,#8ff0b5)'></i></div>"
           % (n_done, n_run, n_fail, pct))
    return bar + rows


# ---------------------------------------------------------------- dashboard bits

def latest_status():
    """The most recent status/report dict the plugin posted."""
    for _, _, body in reversed(entries):
        try:
            j = json.loads(body)
            if isinstance(j, dict) and j.get("mode"):
                return j
        except Exception:
            continue
    return None


def latest_plans():
    """The most recent album plans across any status/report entry."""
    for _, _, body in reversed(entries):
        try:
            j = json.loads(body)
            if isinstance(j, dict) and j.get("plans"):
                return j["plans"]
        except Exception:
            continue
    return None


def latest_actions():
    """The most recent `actions` list across any status/report entry."""
    for _, _, body in reversed(entries):
        try:
            j = json.loads(body)
            if isinstance(j, dict) and j.get("actions"):
                return j["actions"]
        except Exception:
            continue
    return None


def last_action_html():
    """One-line 'last action' ticker for the Now-doing hero."""
    actions = latest_actions()
    if not actions:
        return ""
    last = actions[-1] if isinstance(actions[-1], dict) else None
    if not last or not last.get("text"):
        return ""
    text = last["text"]
    age = ""
    ts = last.get("ts")
    if ts:
        try:
            age = " · %ds ago" % max(0, int(time.time()) - int(ts))
        except (TypeError, ValueError):
            pass
    return ("<div class='last-action'><b>last action</b>%s · %s</div>" % (age, esc(text)))


def _action_chips(actions):
    """Small stage chips derived from an album's actions (moves / nfo / art /
    lyrics / acoustic / lidarr / audiomuse)."""
    if not actions:
        return ""
    text = " ".join(
        (a.get("text", "") for a in actions if isinstance(a, dict) and a.get("text"))
    ).lower()
    chips = []
    if "moved" in text or "would move" in text:
        chips.append("<span class='chip'>moves</span>")
    if "album.nfo" in text:
        chips.append("<span class='chip'>nfo</span>")
    if "artwork" in text or "cover.jpg" in text:
        chips.append("<span class='chip'>art</span>")
    if "lyrics" in text:
        chips.append("<span class='chip'>lyrics</span>")
    if "acoustic" in text:
        chips.append("<span class='chip'>acoustic</span>")
    if "lidarr" in text:
        chips.append("<span class='chip'>lidarr</span>")
    if "audiomuse" in text:
        chips.append("<span class='chip'>audiomuse</span>")
    return ("<span class='e-chips'>%s</span>" % "".join(chips)) if chips else ""


def actions_html(limit=200):
    """THE transparency view: every distinct album plan ever reported, newest
    first, with each file move's full before -> after path. Scrollable, complete,
    so a dry run shows exactly what an apply run would do to the collection."""
    seen = set()
    items = []
    # Bound the scan to the most recent entries; the plugin appends oldest ->
    # newest and we want newest plans first, so scanning the newest slice is
    # both correct and fast no matter how many events have accumulated.
    for _, _, body in reversed(entries[-5000:]):
        try:
            j = json.loads(body)
            if not isinstance(j, dict) or not j.get("plans"):
                continue
        except Exception:
            continue
        dry = j.get("mode", "") != "apply"
        entry_actions = j.get("actions") or []
        for p in j.get("plans"):
            if not isinstance(p, dict):
                continue
            target = p.get("target", "")
            if not target or target in seen:
                continue
            seen.add(target)
            items.append((p, dry, entry_actions))
            if len(items) >= limit:
                break
        if len(items) >= limit:
            break
    if not items:
        return ("<div class='note'>No plans reported yet - as soon as a run plans "
                "its albums, every file move (before &rarr; after) appears here, "
                "scrollable and complete.</div>")
    kind_label = {"soundtrack": "Soundtrack", "various": "Various", "singles": "Single/Incomplete", "normal": "Album"}
    total_moves = 0
    out = "<div class='actlist'>"
    for p, dry, entry_actions in items:
        kind = kind_label.get(p.get("kind", ""), p.get("kind", "?"))
        album = p.get("album", "") or ""
        artist = p.get("albumArtist", "") or ""
        target = p.get("target", "")
        year = p.get("year")
        moves = p.get("moves") or []
        total_moves += len(moves)
        tag = "<span class='tag wait'>DRY RUN</span>" if dry else "<span class='tag ok'>APPLIED</span>"
        out += "<div class='act'><div class='act-top'>%s <span class='plan-k'>%s</span>" % (tag, esc(kind))
        if artist:
            out += "<span class='plan-a'>%s</span>" % esc(artist)
        out += "<span class='plan-t'>%s</span>" % esc(album or target)
        if year:
            out += " <span class='dim'>%s</span>" % esc(str(year))
        out += "</div>"
        out += _action_chips(entry_actions)
        if target:
            out += "<div class='dim act-target'>target &rarr; /%s</div>" % esc(target)
        if moves:
            out += "<div class='moves'>"
            for m in moves:
                if isinstance(m, dict):
                    out += ("<div class='move'><span class='mv-f'>%s</span>"
                            " &rarr; <span class='mv-t'>%s</span></div>"
                            % (esc(m.get("from", "")), esc(m.get("to", ""))))
            out += "</div>"
        d = p.get("duplicates", 0)
        fl = p.get("fillers", 0)
        if d or fl:
            out += "<div class='dim'>duplicates: %d, fillers: %d</div>" % (d, fl)
        out += "</div>"
    out += "</div>"
    header = ("<div class='ig-sum'><b>%d</b> album plan(s), <b>%d</b> file move(s) - "
              "newest first. Every move shows its full before &rarr; after path."
              % (len(items), total_moves))
    return header + out


def pipeline_html(phase, dry):
    """Horizontal pipeline stepper: Scan -> Verify -> Group -> Plan ->
    Preview/Apply -> Stats, highlighting where the organizer is right now."""
    steps = [("Scan", "scan"), ("Verify", "verify"), ("Group", "group"),
             ("Plan", "plan"), ("Preview" if dry else "Apply", "apply"), ("Stats", "stats")]
    idx = {"scan": 0, "verify": 1, "group": 2, "plan": 4, "stats": 5}.get(phase)
    if idx is None:
        return ""
    out = "<div class='pipe'>"
    for i, (label, _key) in enumerate(steps):
        cls = "done" if i < idx else ("cur" if i == idx else "")
        out += "<div class='step %s'><span class='dot'></span>%s</div>" % (cls, label)
    out += "</div>"
    return out


def _fmt_ms(ms):
    try:
        s = int(ms) // 1000
        return "%d:%02d" % (s // 60, s % 60)
    except (TypeError, ValueError):
        return "0:00"


def _stars_html(stars):
    try:
        stars = float(stars or 0)
    except (TypeError, ValueError):
        stars = 0.0
    s = ""
    for i in range(1, 6):
        if stars >= i - 0.25:
            s += "&#9733;"
        elif stars >= i - 0.75:
            s += "&#189;"
        else:
            s += "&#9734;"
    return s + " <span class='dim'>%s</span>" % stars


def _accumulate_playback(status_j):
    """Accumulate playback data across multiple status posts so the panel
    retains history between refreshes."""
    global _playback_state
    if not status_j:
        return
    # Merge topRated - keep top 20 by rating
    existing = {t.get("name"): t for t in _playback_state.get("topRated", [])}
    for t in (status_j.get("topRated") or []):
        if isinstance(t, dict) and t.get("name"):
            name = t["name"]
            if name not in existing or t.get("stars", 0) > existing[name].get("stars", 0):
                existing[name] = t
    _playback_state["topRated"] = sorted(existing.values(), key=lambda x: x.get("stars", 0), reverse=True)[:20]
    # Accumulate filtered items (keep last 50)
    filtered = _playback_state.get("filtered", [])
    new_filtered = status_j.get("filtered") or []
    if new_filtered:
        filtered = (new_filtered + filtered)[:50]
        _playback_state["filtered"] = filtered
    # Accumulate stats
    _playback_state["plays"] = _playback_state.get("plays", 0) + (status_j.get("playsDelta") or 0)
    _playback_state["skips"] = _playback_state.get("skips", 0) + (status_j.get("skipsDelta") or 0)


def playback_html(status_j):
    """'Playback' panel: what is playing right now, playcounts + star ratings,
    and what the filter proxy has been filtering/skipping. Uses accumulated data
    so the panel retains history between status posts."""
    if not status_j:
        return "<div class='card now'><h2>Playback</h2><div class='note'>Waiting for playback data&hellip;</div></div>"
    out = "<div class='card now'><h2>Playback</h2>"

    # What is playing right now.
    np = status_j.get("nowPlaying")
    if isinstance(np, list) and np:
        rows = ""
        for e in np[:8]:
            if not isinstance(e, dict):
                continue
            pos = e.get("positionMs", 0)
            dur = e.get("duration", 0)
            pct = int(pos / (dur * 1000.0) * 100.0) if dur and dur > 0 else 0
            rows += ("<div class='np'><span class='np-dot'></span>"
                     "<span class='np-a'>%s</span> <b>%s</b>"
                     "<span class='dim'>%s</span>"
                     "<span class='dim np-pos'>%s / %s (%d%%)</span></div>") % (
                esc(e.get("artist", "")), esc(e.get("title", "") or "?"),
                esc(e.get("album", "")), _fmt_ms(pos), _fmt_ms(dur * 1000), pct)
        out += "<div class='np-head'>Now playing</div>" + rows
    else:
        out += "<div class='np-head'>Now playing</div><div class='note'>Nothing is playing right now.</div>"

    # Playcounts + star ratings - merge current with accumulated
    current_tr = {t.get("name"): t for t in (status_j.get("topRated") or []) if isinstance(t, dict)}
    acc_tr = {t.get("name"): t for t in _playback_state.get("topRated", []) if isinstance(t, dict)}
    acc_tr.update(current_tr)  # current takes precedence
    merged_tr = sorted(acc_tr.values(), key=lambda x: x.get("stars", 0), reverse=True)[:10]
    if merged_tr:
        rows = ""
        for t in merged_tr:
            rows += ("<div class='tr'><span class='tr-stars'>%s</span>"
                     "<span class='tr-name'>%s</span>"
                     "<span class='dim'>%d plays</span></div>") % (
                _stars_html(t.get("stars", 0)), esc(t.get("name", "")),
                int(t.get("plays", 0)))
        out += "<div class='np-head'>Playcounts &amp; star ratings</div>" + rows
    else:
        out += ("<div class='np-head'>Playcounts &amp; star ratings</div>"
                "<div class='note'>No ratings yet - they build up as music plays.</div>")

    # What the filter proxy has been dropping - use accumulated list
    proxy = _fetch_json("nd-organizer-proxy", 4534, "/status", _sidecar_status)
    filtered = (proxy or {}).get("filtered") or _playback_state.get("filtered", [])
    if filtered:
        rows = ""
        for it in filtered[:15]:
            if not isinstance(it, dict):
                continue
            reason = it.get("reason", "")
            chip = "<span class='chip'>%s</span>" % esc(reason) if reason in ("keyword", "excluded") else ""
            rows += ("<div class='fh'><span class='ts'>%s</span><b>%s</b>"
                     "<span class='dim'>%s</span>%s</div>") % (
                _fmt_ts(it.get("ts")), esc(it.get("song", "") or it.get("id", "?")),
                esc(it.get("artist", "")), chip)
        out += "<div class='np-head'>Recently filtered by the proxy</div>" + rows
    else:
        out += ("<div class='np-head'>Recently filtered by the proxy</div>"
                "<div class='note'>Nothing filtered recently.</div>")

    # Cumulative stats
    plays = status_j.get("plays", 0) or _playback_state.get("plays", 0)
    skips = status_j.get("skips", 0) or _playback_state.get("skips", 0)
    ratings = status_j.get("ratings", 0)
    out += ("<div class='sc-stats'><span>plays observed <b>%s</b></span>"
            "<span>skips observed <b>%s</b></span>"
            "<span>ratings published <b>%s</b></span></div>") % (plays, skips, ratings)
    out += "</div>"
    return out


def now_panel(j):
    """The 'Current activity' hero: a plain-English line about the current action, a
    pipeline stepper, run/batch chips, rollback info, warnings and the per-library
    counts. This is the at-a-glance answer to 'what is it doing to my files?'."""
    if not j:
        return ("<div class='card now'><h2>Live status</h2>"
                "<div class='note'>Waiting for the plugin to report&hellip;</div></div>")
    mode = j.get("mode", "")
    dry = mode != "apply"
    phase = j.get("phase", "")

    # Big state pill.
    if j.get("rollbackOfRun"):
        state, sc = "Rolling back", "run"
    elif j.get("inProgress"):
        state, sc = "Working", "run"
    elif j.get("deferredUntilIdle"):
        state, sc = "Waiting for idle", "wait"
    elif j.get("metaSkipped"):
        state, sc = "Skipped", "warn"
    else:
        state, sc = "Idle", "ok"

    # Plain-English current action.
    now = "Idle - waiting for the next scheduled run."
    if j.get("metaSkipped"):
        now = ("Run skipped - <b>%s</b> is offline, so required metadata is unavailable. "
               "Retrying later instead of organizing on missing data.") % esc(j["metaSkipped"])
    elif phase == "scan":
        now = "Scanning the library - <b>%s</b> files indexed so far (this chunk: <b>%s</b>)." % (
            int(j.get("filesScanned", 0)), int(j.get("chunkSize", 0)))
        cf = j.get("currentFile")
        if cf:
            now += "<br>Currently reading: <span class='now-file'>%s</span>" % esc(cf)
    elif phase == "verify":
        now = "Verifying track identities (MusicBrainz / ISRC / AcoustID)&hellip;"
    elif phase == "group":
        now = "Grouping files into albums by their metadata&hellip;"
    elif phase == "plan" or j.get("plans"):
        libs = j.get("libraries") or []
        am = sum(int(l.get("albumsToMove", 0)) for l in libs)
        fm = sum(int(l.get("fileMoves", 0)) for l in libs)
        if dry:
            now = "Dry-run preview - <b>%s</b> album(s) would change with <b>%s</b> file move(s). Nothing is written." % (am, fm)
        else:
            now = "Applying changes - <b>%s</b> album(s), <b>%s</b> file move(s)." % (am, fm)
    elif phase == "stats":
        now = ("Playback stats - <b>%s</b> plays, <b>%s</b> skips, <b>%s</b> top picks, "
               "<b>%s</b> skip-heavy limited, <b>%s</b> rating(s) published.") % (
            int(j.get("plays", 0)), int(j.get("skips", 0)), int(j.get("topPicks", 0)),
            int(j.get("filtered", 0)), int(j.get("ratings", 0)))
    if j.get("deferredUntilIdle"):
        now = "Run deferred because playback is active - retrying automatically."
    if j.get("rollbackOfRun"):
        now = "Rolling back run <b>%s</b> - restoring files, folders and album.nfo from backup." % esc(j.get("rollbackOfRun"))

    batch = ""
    b = j.get("batch")
    if isinstance(b, dict) and b.get("total"):
        batch = "<span class='tag'>batch %d/%d</span>" % (int(b.get("index", 0)) + 1, int(b["total"]))
    run = ""
    if j.get("runId"):
        run = "<span class='tag mode'>run %s</span>" % esc(j["runId"])
    if j.get("rollbackOfRun"):
        run += "<span class='tag'>rollback</span>"

    html = "<div class='card now'><h2>Current activity <span class='meta'>%s</span></h2>" % (
        esc(datetime.now().strftime("%H:%M:%S")))
    html += "<div class='now-top'><span class='pill %s'>%s</span><span class='now-line'>%s</span> %s %s</div>" % (
        sc, state, now, batch, run)
    html += last_action_html()
    html += pipeline_html(phase, dry)
    if phase == "scan":
        html += "<div class='bar'><i></i></div>"
    if j.get("runId") and not j.get("rollbackOfRun"):
        html += ("<div class='rollback'>Undo this run? Set <b>rollbackRunId</b> = "
                 "<code>%s</code> in the plugin settings, then run a pass. Files, folders and "
                 "album.nfo are restored from backup.</div>") % esc(j["runId"])
    warns = j.get("warnings")
    if isinstance(warns, list) and warns:
        html += ("<div class='warn'><b>Configuration &amp; connectivity warnings:</b><ul>%s</ul></div>"
                 % "".join("<li>%s</li>" % esc(str(w)) for w in warns))
    if j.get("feedback"):
        html += "<div class='feedback'>%s</div>" % esc(j["feedback"])
    libs = j.get("libraries")
    if isinstance(libs, list) and libs:
        html += ("<table><tr><th>Library</th><th>Albums found</th><th>To move</th>"
                 "<th>File moves</th><th>Kept</th><th>Skipped</th><th>Dupes</th></tr>")
        for lib in libs:
            html += ("<tr><td>%s <span class='dim'>(id %s)</span></td><td>%s</td><td><b>%s</b></td>"
                     "<td>%s</td><td>%s</td><td>%s</td><td>%s</td></tr>") % (
                esc(lib.get("name", "")), lib.get("id", ""), lib.get("albumsFound", 0),
                lib.get("albumsToMove", 0), lib.get("fileMoves", 0), lib.get("kept", 0),
                lib.get("skipped", 0), lib.get("duplicates", 0))
        html += "</table>"
        html += "<div class='totals'>Total to move: <b>%s</b> &middot; file moves: <b>%s</b></div>" % (
            j.get("totalAlbumsToMove", 0), j.get("totalFileMoves", 0))
    html += "</div>"
    return html


def entry_summary(body):
    try:
        j = json.loads(body)
        if not isinstance(j, dict) or "mode" not in j:
            return None
    except Exception:
        return None
    parts = [j.get("mode", "")]
    b = j.get("batch")
    if isinstance(b, dict) and b.get("total"):
        parts.append("batch %d/%d" % (int(b.get("index", 0)) + 1, int(b["total"])))
    if j.get("deferredUntilIdle"):
        parts.append("deferred (idle)")
    libs = j.get("libraries")
    if isinstance(libs, list) and libs:
        parts.append("%s to move" % libs[0].get("albumsToMove", 0))
        parts.append("%s file moves" % libs[0].get("fileMoves", 0))
    return " | ".join(parts)


def current_mode():
    """Latest mode the plugin reported (dryRun / apply), so text-only entries
    can be labelled correctly."""
    for _, _, body in reversed(entries):
        try:
            j = json.loads(body)
            if isinstance(j, dict) and j.get("mode"):
                return j["mode"]
        except Exception:
            continue
    return None


def mode_chip(mode):
    return "<span class='tag ok'>APPLY</span>" if mode == "apply" else "<span class='tag wait'>DRY RUN</span>"


def plans_html(plans):
    """Structured album plans: kind, artist/album, target folder, every move."""
    if not plans:
        return ""
    kind_label = {"soundtrack": "Soundtrack", "various": "Various", "singles": "Single/Incomplete", "normal": "Album"}
    out = "<div class='plans'><b>Albums in this batch:</b>"
    for p in plans:
        if not isinstance(p, dict):
            continue
        kind = kind_label.get(p.get("kind", ""), p.get("kind", "?"))
        target = p.get("target", "") or ""
        album = p.get("album", "") or ""
        artist = p.get("albumArtist", "") or ""
        year = p.get("year")
        title = album or artist or target or "?"
        out += "<div class='plan'><span class='plan-k'>%s</span>" % esc(kind)
        if artist:
            out += "<span class='plan-a'>%s</span>" % esc(artist)
        out += "<span class='plan-t'>%s</span>" % esc(title)
        if year:
            out += " <span class='dim'>%s</span>" % esc(str(year))
        if target:
            out += " <span class='dim'>target /%s</span>" % esc(target)
        mv = p.get("moves") or []
        if mv:
            out += "<div class='moves'>"
            for m in mv:
                if not isinstance(m, dict):
                    continue
                out += ("<div class='move'><span class='mv-f'>%s</span>"
                        " &rarr; <span class='mv-t'>/%s</span></div>"
                        % (esc(m.get("from", "")), esc(m.get("to", ""))))
            out += "</div>"
        if not mv:
            out += "<div class='dim'>no file moves</div>"
        d = p.get("duplicates", 0)
        fl = p.get("fillers", 0)
        if d or fl:
            out += "<div class='dim'>%s</div>" % esc("duplicates: %d, fillers: %d" % (d, fl))
        out += "</div>"
    out += "</div>"
    return out


def recent_actions_html(limit=25):
    """Compact feed of the most recent discrete actions across all runs,
    newest first (memory + webhook.log only - no extra persistence)."""
    items = []
    for _, _, body in reversed(entries[-5000:]):
        try:
            j = json.loads(body)
        except Exception:
            continue
        if not isinstance(j, dict) or not j.get("actions"):
            continue
        dry = j.get("mode", "") != "apply"
        for a in reversed(j.get("actions")):
            if isinstance(a, dict) and a.get("text"):
                items.append((a.get("ts"), a["text"], dry))
        if len(items) >= limit:
            break
    items = items[:limit]
    if not items:
        return "<div class='note'>No actions recorded yet - moves, nfo, art, lyrics and acoustic writes appear here as they happen.</div>"
    rows = ""
    for ts, text, dry in items:
        chip = "<span class='chip wait'>DRY RUN</span>" if dry else "<span class='chip ok'>APPLY</span>"
        rows += ("<div class='ra'><span class='ts'>%s</span>%s <span class='ra-text'>%s</span></div>"
                 % (_fmt_ts(ts), chip, esc(text)))
    return "<div class='ralist'>%s</div>" % rows


def activity_entry(ts, path, body, fallback_mode):
    """One Activity row, rendered richly: mode chip (DRY RUN/APPLY on every
    entry), phase/batch/run chips, structured album plans with old->new paths,
    warnings, and the raw payload behind a fold."""
    issue = None
    chips = []
    detail = ""
    summary = None
    try:
        j = json.loads(body)
        if not isinstance(j, dict):
            j = None
    except Exception:
        j = None

    if j and "mode" in j:
        mode = j.get("mode", "")
        chips.append(mode_chip(mode))
        b = j.get("batch")
        if isinstance(b, dict) and b.get("total"):
            chips.append("<span class='chip'>batch %d/%d</span>" % (int(b.get("index", 0)) + 1, int(b["total"])))
        if j.get("deferredUntilIdle"):
            chips.append("<span class='chip wait'>deferred - waiting for idle</span>")
        if j.get("metaSkipped"):
            chips.append("<span class='chip wait'>skipped - %s offline</span>" % esc(j["metaSkipped"]))
        if j.get("phase"):
            chips.append("<span class='chip'>%s</span>" % esc(str(j["phase"]).upper()))
        if j.get("runId"):
            chips.append("<span class='chip'>run %s</span>" % esc(j["runId"]))
        if j.get("rollbackOfRun"):
            chips.append("<span class='chip'>rollback of %s</span>" % esc(j["rollbackOfRun"]))
        bad = [i.get("name", "?") for i in (j.get("integrations") or [])
               if i.get("state") in ("unreachable", "authFailed")]
        warns = j.get("warnings") or []
        if bad:
            issue = "ISSUES: " + ", ".join(map(str, bad))
        elif warns:
            issue = "WARNINGS: %d" % len(warns)
        libs = j.get("libraries")
        if isinstance(libs, list) and libs and isinstance(libs[0], dict):
            l = libs[0]
            summary = "%s album(s) found, %s to move, %s file moves, %s duplicate(s)" % (
                l.get("albumsFound", 0), l.get("albumsToMove", 0),
                l.get("fileMoves", 0), l.get("duplicates", 0))
        plans = j.get("plans")
        if isinstance(plans, list) and plans:
            detail += plans_html(plans)
        if j.get("kind") == "report" and j.get("text"):
            detail += "<details open><summary>report text</summary><pre>%s</pre></details>" % esc(j["text"])
        elif j.get("feedback"):
            detail += "<div class='feedback'>%s</div>" % esc(j["feedback"])
        if warns:
            detail += ("<div class='warn'><b>Warnings:</b><ul>%s</ul></div>"
                       % "".join("<li>%s</li>" % esc(str(w)) for w in warns))
        # Journal-style summary instead of raw JSON dump
        journal = []
        if j.get("runId"):
            journal.append("run %s" % esc(j["runId"]))
        if j.get("phase"):
            journal.append("phase: %s" % esc(j["phase"]))
        if j.get("batch"):
            b = j["batch"]
            if isinstance(b, dict) and b.get("total"):
                journal.append("batch %d/%d" % (int(b.get("index", 0)) + 1, int(b["total"])))
        if j.get("totalAlbumsToMove"):
            journal.append("%s album(s) to move" % j["totalAlbumsToMove"])
        if j.get("totalFileMoves"):
            journal.append("%s file move(s)" % j["totalFileMoves"])
        if j.get("plans"):
            journal.append("%s plan(s)" % len(j["plans"]))
        if j.get("actions"):
            journal.append("%s action(s)" % len(j["actions"]))
        if journal:
            detail += "<div class='dim journal'>%s</div>" % " &middot; ".join(journal)
        detail += "<details><summary>raw</summary><pre>%s</pre></details>" % esc(body)
    else:
        # Plain text / legacy report: label it with the latest known mode.
        if fallback_mode == "dryRun":
            chips.append("<span class='chip wait'>DRY RUN</span>")
        summary = entry_summary(body) or "report/log"
        detail = "<details open><summary>report / log</summary><pre>%s</pre></details>" % esc(body)

    cls = " class='e issue'" if issue else " class='e'"
    chips_html = ("<span class='e-chips'>%s</span>" % "".join(chips)) if chips else ""
    html = ("<div%s><span class='ts'>%s</span> <span class='m'>POST</span> "
            "<span class='p'>%s</span>%s") % (cls, ts, esc(path), chips_html)
    if issue:
        html += "<span class='chip issue'>%s</span>" % esc(issue)
    if summary:
        html += "<div class='sum'>%s</div>" % esc(summary)
    html += detail + "</div>"
    return html


class Handler(http.server.BaseHTTPRequestHandler):
    def _read_body(self):
        try:
            n = int(self.headers.get("Content-Length", 0))
        except ValueError:
            n = 0
        return self.rfile.read(n).decode("utf-8", "replace") if n > 0 else ""

    def do_POST(self):
        body = self._read_body()
        ts = datetime.now().strftime("%Y-%m-%d %H:%M:%S")
        # Internet radio: add a station directly to Navidrome radio table.
        if self.path.rstrip("/").endswith("/radio-add"):
            log.info("radio-add: handler entered, body=%d bytes", len(body))
            try:
                data = json.loads(body) if body else {}
                stations = data.get("stations") or []
                if not stations:
                    log.warning("radio-add: no stations in request body")
                    self.send_response(400); self.send_header("Content-Length", "0"); self.end_headers()
                    return
                s = stations[0]
                name = s.get("name", "")
                url = s.get("url", "")
                homepage = s.get("homepage", "")
                if not name or not url:
                    log.warning("radio-add: missing name or url")
                    self.send_response(400); self.send_header("Content-Length", "0"); self.end_headers()
                    return
                log.info("radio-add: adding %s", name)
                added, skipped, errors = radio_add_stations([{"name": name, "url": url, "homepage": homepage}])
                log.info("radio-add: added=%d skipped=%d errors=%s", added, skipped, errors)
            except Exception as e:
                log.warning("radio-add failed: %s", e)
                self._send(502, {"ok": False, "error": str(e)})
                return
            self._send(200, {"ok": True, "added": added, "skipped": skipped, "errors": errors})
            return
        if self.path.rstrip("/").endswith("/radio-remove"):
            try:
                req = json.loads(body) if body else {}
                name = req.get("name", "")
                url = req.get("url", "")
                if not name and not url:
                    self._send(400, {"ok": False, "error": "name or url required"})
                    return
                conn = radio_db_connect()
                cur = conn.cursor()
                cur.execute("DELETE FROM radio WHERE name = ? OR stream_url = ?", (name, url))
                deleted = cur.rowcount
                conn.commit()
                conn.close()
                log.info("radio-remove: deleted %d station(s) name='%s'", deleted, name)
                self._send(200, {"ok": True, "deleted": deleted})
            except Exception as e:
                log.warning("radio-remove failed: %s", e)
                self._send(502, {"ok": False, "error": str(e)})
            return
        if self.path.rstrip("/").endswith("/radio-rename"):
            try:
                req = json.loads(body) if body else {}
                old_name = req.get("old_name", "")
                new_name = req.get("new_name", "")
                url = req.get("url", "")
                if not old_name or not new_name:
                    self._send(400, {"ok": False, "error": "old_name and new_name required"})
                    return
                conn = radio_db_connect()
                cur = conn.cursor()
                ts = datetime.datetime.utcnow().strftime("%Y-%m-%d %H:%M:%S.%f")
                if url:
                    cur.execute("UPDATE radio SET name = ?, updated_at = ? WHERE name = ? OR stream_url = ?",
                                (new_name, ts, old_name, url))
                else:
                    cur.execute("UPDATE radio SET name = ?, updated_at = ? WHERE name = ?",
                                (new_name, ts, old_name))
                updated = cur.rowcount
                conn.commit()
                conn.close()
                log.info("radio-rename: %d station(s) '%s' -> '%s'", updated, old_name, new_name)
                self._send(200, {"ok": True, "updated": updated})
            except Exception as e:
                log.warning("radio-rename failed: %s", e)
                self._send(502, {"ok": False, "error": str(e)})
            return
        # Force rescan: post a signal to the log so next scheduled run re-scans.
        if self.path.rstrip("/").endswith("/force-rescan"):
            log.info("force-rescan: handler entered, body=%d bytes", len(body))
            try:
                ts = datetime.now().strftime("%Y-%m-%d %H:%M:%S")
                signal = json.dumps({
                    "ts": int(time.time()),
                    "mode": current_mode(),
                    "inProgress": False,
                    "feedback": "Force rescan requested - next scheduled run will re-scan from scratch.",
                    "integrations": [],
                })
                entries.append((ts, "/force-rescan", signal))
                self._send(200, {"ok": True, "message": "rescan signal posted"})
            except Exception as e:
                log.warning("force-rescan failed: %s", e)
                self._send(500, {"ok": False, "error": str(e)})
            return
        # Playlist: save / delete / list / deploy preset
        if self.path.rstrip("/").endswith("/playlist-save"):
            try:
                n = int(self.headers.get("Content-Length", 0))
                raw = self.rfile.read(n) if n > 0 else b"{}"
                req = json.loads(raw or "{}")
                ok, filename, err = save_playlist(
                    req.get("name", ""),
                    req.get("comment", ""),
                    req.get("nsp", {}))
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                body = json.dumps({"ok": ok, "filename": filename, "error": err}).encode()
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self._wfile_write(body)
            except Exception as e:
                log.warning("playlist-save failed: %s", e)
                self._send(500, {"error": str(e)})
            return
        if self.path.rstrip("/").endswith("/playlist-delete"):
            try:
                n = int(self.headers.get("Content-Length", 0))
                raw = self.rfile.read(n) if n > 0 else b"{}"
                req = json.loads(raw or "{}")
                ok, err = delete_playlist(req.get("file", ""))
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                body = json.dumps({"ok": ok, "error": err}).encode()
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self._wfile_write(body)
            except Exception as e:
                log.warning("playlist-delete failed: %s", e)
                self._send(500, {"error": str(e)})
            return
        # Sidecar heartbeat? Body is {"service": "...", "ts": ...} with no "mode".
        try:
            j = json.loads(body) if body else {}
        except Exception:
            j = {}
        if isinstance(j, dict) and j.get("service") and "mode" not in j:
            services[j["service"]] = time.time()
            log.info("heartbeat from %s", j["service"])
            self.send_response(200)
            self.send_header("Content-Length", "2")
            self.end_headers()
            self._wfile_write(b"ok")
            return
        summary = entry_summary(body) or "report/log"
        log.info("received POST %s from %s (%d bytes) - %s", self.path, self.client_address[0], len(body), summary)
        entries.append((ts, self.path, body))
        if len(entries) > MAX_ENTRIES:
            del entries[: len(entries) - MAX_ENTRIES]
        # Accumulate playback data for the Playback panel
        try:
            _accumulate_playback(json.loads(body))
        except Exception:
            pass
        # Never crash the request on a log-file problem: create the directory if
        # needed and fall back to memory-only if it still can't be written.
        try:
            logdir = os.path.dirname(LOGFILE)
            if logdir:
                os.makedirs(logdir, exist_ok=True)
            with open(LOGFILE, "a", encoding="utf-8") as f:
                f.write("[%s] POST %s - %s\n" % (ts, self.path, body))
        except Exception as e:
            log.warning("could not write log file %s: %s", LOGFILE, e)
        self.send_response(200)
        self.send_header("Content-Length", "3")
        self.end_headers()
        self._wfile_write(b"ok\n")

    def do_GET(self):
        global last_any_request, _render_deadline
        last_any_request = time.time()
        # Bound the whole render (~3.5s): sidecar probes that run past this are
        # skipped (cached/None) so the page never blocks on unreachable services.
        _render_deadline = time.time() + 5.0
        try:
            self._render()
        finally:
            _render_deadline = 0.0

    def _render(self):
        if self.path.startswith("/health"):
            data = json.dumps({
                "ok": True, "service": "nd-organizer-webhook", "port": PORT,
                "events": len(entries), "log": LOGFILE,
            }).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self._wfile_write(data)
            return
        if self.path.startswith("/status"):
            data = json.dumps({
                "service": "nd-organizer-webhook",
                "ok": True,
                "uptime": int(time.time() - STARTED),
                "events": len(entries),
                "log": LOGFILE,
                "stats": {
                    "entries": len(entries),
                    "services": len(services),
                    "sidecars_known": len(SIDECAR_LOG_PORTS),
                },
            }).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self._wfile_write(data)
            return
        if self.path.startswith("/radio-list"):
            try:
                stations = radio_list_stations()
                data = json.dumps({"ok": True, "stations": stations}).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self._wfile_write(data)
            except Exception as e:
                log.warning("radio-list failed: %s", e)
                err = json.dumps({"ok": False, "error": str(e), "stations": []}).encode()
                self.send_response(502)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(err)))
                self.end_headers()
                self._wfile_write(err)
            return
        if self.path.startswith("/radio-lookup"):
            # AJAX: search Radio-Browser API locally and return JSON directly.
            qs = urllib.parse.parse_qs(urllib.parse.urlparse(self.path).query)
            q = (qs.get("q") or [""])[0]
            stype = (qs.get("type") or ["byname"])[0]
            limit = int((qs.get("limit") or ["20"])[0])
            try:
                results = radio_search(q, stype, limit)
                data = json.dumps({"ok": True, "results": results}).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self._wfile_write(data)
            except Exception as e:
                log.warning("radio-search failed: %s", e)
                err = json.dumps({"ok": False, "error": str(e)}).encode()
                self.send_response(502)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(err)))
                self.end_headers()
                self._wfile_write(err)
            return
        # Playlist: list all .nsp files (AJAX).
        if self.path.startswith("/playlist-list"):
            try:
                pl = list_playlists()
                data = json.dumps({"ok": True, "playlists": pl}).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self._wfile_write(data)
            except Exception as e:
                self._send(500, {"error": str(e)})
            return
        # Playlist: deploy a preset (AJAX).
        if self.path.startswith("/playlist-presets"):
            try:
                qs = urllib.parse.parse_qs(urllib.parse.urlparse(self.path).query)
                action = (qs.get("action") or [""])[0]
                name = (qs.get("name") or [""])[0]
                if action == "deploy" and name:
                    preset = next((p for p in SMART_PRESETS if p["name"] == name), None)
                    if preset:
                        ok, filename, err = save_playlist(preset["name"], preset.get("comment", ""), preset)
                        data = json.dumps({"ok": ok, "filename": filename, "error": err}).encode()
                    else:
                        data = json.dumps({"ok": False, "error": "preset not found"}).encode()
                else:
                    data = json.dumps({"ok": True, "presets": [{"name": p["name"], "comment": p["comment"]} for p in SMART_PRESETS]}).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self._wfile_write(data)
            except Exception as e:
                self._send(500, {"error": str(e)})
            return

        status_j = latest_status()
        now_html = now_panel(status_j)
        mode = current_mode()
        banner = ""
        if mode == "dryRun":
            banner = ("<div class='banner dry'><b>DRY RUN</b> - the plugin is NOT "
                      "modifying any files. Everything below shows exactly what an "
                      "apply run WOULD do. Switch the plugin mode to <b>apply</b> "
                      "to execute it (rollback data is kept for every run).</div>")
        elif mode == "apply":
            banner = ("<div class='banner on'><b>APPLY mode</b> - the plugin may "
                      "move files. Rollback data is kept for every run.</div>")
        # Sidecar checks FIRST — they're the only network calls, do them
        # before the budget is consumed by rendering other panels.
        sidecars_html = sidecar_logs_html()
        albums_html = actions_html()
        rows = ""
        # Render only the most recent entries - iterating all 100k+ events on
        # every refresh is what was stalling the dashboard. 500 is plenty for
        # the activity feed; older events stay in the log file.
        for ts, path, body in reversed(entries[:500]):
            rows += activity_entry(ts, path, body, mode)
        if not rows:
            rows = "<div class='note'>Waiting for the plugin to POST its status/reports &hellip;</div>"

        plugin_state = "connected" if entries else "no activity yet"
        updated = datetime.now().strftime("%H:%M:%S")

        page = (PAGE
                .replace("__COUNT__", str(len(entries)))
                .replace("__PLUGIN__", plugin_state)
                .replace("__UPDATED__", updated)
                .replace("__LOG__", esc(LOGFILE))
                .replace("__MODE__", esc(mode or "unknown"))
                .replace("__BANNER__", banner)
                .replace("__INTEGRATIONS__", integrations_html())
                .replace("__NOW__", now_html)
                .replace("__PLAYBACK__", playback_html(status_j))
                .replace("__RADIO__", radio_html())
                .replace("__PLAYLISTS__", playlist_html())
                .replace("__ALBUMS__", albums_html)
                .replace("__TASKS__", tasks_html())
                .replace("__SIDECARS__", sidecars_html)
                .replace("__RECENT__", recent_actions_html())
                .replace("__ROWS__", rows))
        data = page.encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self._wfile_write(data)

    def _wfile_write(self, data):
        """Write a response body, swallowing broken-pipe/reset errors - a client
        (browser tab, heartbeat sender) that disconnects mid-response is normal
        and shouldn't dump a traceback into the logs."""
        try:
            self.wfile.write(data)
        except (BrokenPipeError, ConnectionResetError, OSError):
            pass

    def log_message(self, *a):
        pass


PAGE = """<!DOCTYPE html><html><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>nd-organizer</title>
<link rel="icon" href="https://raw.githubusercontent.com/Lunatixz/nd-organizer/main/images/icon.png">
<style>
:root{color-scheme:dark;--bg:#0a0e1a;--surface:#111827;--surface2:#1a2332;--border:#1e2d3d;--border2:#2a3a4d;--text:#e2e8f0;--text2:#64748b;--accent:#00d4ff;--green:#22c55e;--green-bg:rgba(34,197,94,.12);--red:#ef4444;--red-bg:rgba(239,68,68,.1);--yellow:#eab308;--yellow-bg:rgba(234,179,8,.1);--blue:#00d4ff;--blue-bg:rgba(0,212,255,.1);--purple:#a855f7;--purple-bg:rgba(168,85,247,.1);--radius:8px;--radius-lg:12px}
*{box-sizing:border-box}
body{background:var(--bg);color:var(--text);font:14px/1.6 -apple-system,"Segoe UI",Roboto,Helvetica,Arial,sans-serif;margin:0;min-height:100vh;-webkit-font-smoothing:antialiased}
.wrap{max-width:1080px;margin:0 auto;padding:28px 24px 60px}
header{margin-bottom:24px}
h1{font-size:20px;margin:0;color:var(--accent);display:flex;align-items:center;gap:10px;font-weight:600}
h1 .dot{width:8px;height:8px;border-radius:50%;background:var(--green);display:inline-block;animation:pulse 2s infinite}
@keyframes pulse{0%,100%{opacity:1}50%{opacity:.3}}
.spin{width:12px;height:12px;border:2px solid var(--border);border-top-color:var(--accent);border-radius:50%;display:inline-block;animation:spin .7s linear infinite;vertical-align:middle;margin-right:6px}
@keyframes spin{to{transform:rotate(360deg)}}
.sub{color:var(--text2);font-size:12px;margin-top:6px;letter-spacing:.2px}
.badges{display:flex;gap:8px;margin-top:10px;flex-wrap:wrap}
.badge{background:var(--surface);border:1px solid var(--border);border-radius:16px;padding:4px 12px;font-size:12px;color:var(--text)}
.badge b{color:#e6eaf1}
.card{background:var(--surface);border:1px solid var(--border);border-radius:var(--radius-lg);padding:18px 20px;margin-bottom:16px}
details.collapse{background:var(--surface);border:1px solid var(--border);border-radius:var(--radius-lg);margin-bottom:12px;padding:14px 18px}
details.collapse summary{list-style:none;cursor:pointer;font-size:14px;color:#e6eaf1;font-weight:600;margin:0;padding:2px 0;user-select:none}
details.collapse summary::before{content:"\\25B6 ";color:var(--text2);font-size:10px;font-weight:bold;transition:transform .15s;display:inline-block;width:12px}
details.collapse[open] summary::before{transform:rotate(90deg)}
.collapse-body{margin-top:14px}
h2{font-size:14px;margin:0 0 12px;color:#e6eaf1;font-weight:600}
h2 .meta{color:var(--text2);font-size:12px;font-weight:normal;float:right}
.kv{display:flex;gap:8px;flex-wrap:wrap;align-items:center;margin-bottom:10px}
.tag{background:var(--surface2);border:1px solid var(--border);border-radius:14px;padding:3px 10px;font-size:11px;color:var(--text);font-weight:500}
.tag.run{background:var(--yellow-bg);color:var(--yellow);border-color:rgba(210,153,34,.25)}
.tag.wait{background:var(--yellow-bg);color:var(--yellow);border-color:rgba(210,153,34,.25)}
.tag.ok{background:var(--green-bg);color:var(--green);border-color:rgba(63,185,80,.25)}
.tag.bad{background:var(--red-bg);color:var(--red);border-color:rgba(248,81,73,.25)}
.tag.mode{background:var(--blue-bg);color:var(--blue);border-color:rgba(88,166,255,.25)}
.rollback{margin-top:10px;background:var(--blue-bg);border:1px solid rgba(88,166,255,.2);border-radius:var(--radius);padding:10px 14px;color:var(--blue);font-size:13px}
.rollback code{background:var(--bg);border:1px solid var(--border);border-radius:4px;padding:2px 8px;color:#e6f0ff;font-size:12px}
.plans{margin-top:10px;font-size:13px}
.plan{background:var(--surface2);border:1px solid var(--border);border-radius:var(--radius);padding:10px 14px;margin-top:8px}
.plan-k{display:inline-block;background:var(--blue-bg);color:var(--blue);border:1px solid rgba(88,166,255,.2);border-radius:4px;padding:2px 8px;font-size:11px;font-weight:600;margin-right:8px}
.plan-t{font-weight:600;color:#e6eaf1;margin-right:10px}
.moves{margin-top:6px;font-size:12px}
.move{padding:2px 0;color:var(--text)}
.mv-f{color:#f8857a}
.mv-t{color:var(--green)}
.tk{display:flex;align-items:center;gap:8px;padding:7px 0;border-bottom:1px solid var(--border);font-size:13px}
.tk-ts{color:var(--text2);font-size:12px;width:64px;flex-shrink:0}
.tk-kind{font-weight:600;color:#e6eaf1;min-width:70px}
.tk-msg{color:var(--green);flex:1;word-break:break-word}
table{width:100%;border-collapse:collapse;font-size:13px;margin:8px 0}
th{text-align:left;color:var(--text2);font-weight:500;padding:6px 10px;border-bottom:1px solid var(--border);font-size:12px;text-transform:uppercase;letter-spacing:.3px}
td{padding:6px 10px;border-bottom:1px solid var(--border)}
.dim{color:var(--text2);font-size:12px}.totals{margin-top:8px;color:var(--text)}
.note{color:var(--text2);font-size:13px;font-style:italic}
.warn{background:var(--yellow-bg);border:1px solid rgba(210,153,34,.25);border-radius:var(--radius);padding:10px 14px;margin-top:10px;color:var(--yellow);font-size:13px}
.warn ul{margin:6px 0 0;padding-left:18px}
.integrations{display:grid;grid-template-columns:repeat(auto-fill,minmax(180px,1fr));gap:6px}
.ig{background:var(--surface2);border:1px solid var(--border);border-radius:var(--radius);padding:8px 10px}
.ig-top{display:flex;justify-content:space-between;align-items:center;gap:4px;margin-bottom:2px}
.ig-name{font-weight:600;color:#e6eaf1;font-size:12px}
.ig-state{font-size:10px;font-weight:600;padding:2px 8px;border-radius:10px}
.ig-state.ok{background:var(--green-bg);color:var(--green)}
.ig-state.warn{background:var(--yellow-bg);color:var(--yellow)}
.ig-state.bad{background:var(--red-bg);color:var(--red)}
.ig-state.authfail{background:var(--red-bg);color:var(--red)}
.ig-state.dim{background:var(--surface2);color:var(--text2);border:1px solid var(--border)}
.ig .dim{font-size:10px;word-break:break-all}
.ig-sum{margin:0 0 6px;color:var(--text);font-size:12px}
.sc{background:var(--surface2);border:1px solid var(--border);border-radius:var(--radius);padding:10px 12px;margin-bottom:8px}
.sc.off{opacity:.5}
.sc-top{display:flex;justify-content:space-between;align-items:center;gap:6px;margin-bottom:4px;font-size:13px}
.sc-top b{color:#e6eaf1}
.sc-stats{display:flex;flex-wrap:wrap;gap:4px 14px;font-size:11px;color:var(--text2);margin-bottom:4px}
.sc-stats b{color:#e6eaf1}
.fhist{max-height:220px;overflow:auto}
.fh{display:flex;align-items:center;gap:8px;padding:4px 0;border-bottom:1px solid var(--border);font-size:13px;color:var(--text)}
.fh b{color:#e6eaf1;font-weight:500}
.chip.k{background:var(--blue-bg);color:var(--blue);border:0}
.bar{height:6px;border-radius:4px;background:var(--surface2);overflow:hidden;margin:8px 0}
.bar i{display:block;height:100%;width:100%;background:linear-gradient(90deg,var(--blue),var(--green),var(--blue));background-size:200% 100%;animation:slide 1.4s linear infinite}
.bar.f i{animation:none}
@keyframes slide{0%{background-position:0 0}100%{background-position:200% 0}}
.feedback{background:var(--blue-bg);border:1px solid rgba(88,166,255,.2);border-radius:var(--radius);padding:10px 14px;margin-top:10px;color:var(--blue);font-size:13px}
.alert{border-radius:var(--radius);padding:10px 14px;margin:0 0 14px;font-size:13px;line-height:1.5}
.alert.bad{background:var(--red-bg);border:1px solid rgba(248,81,73,.25);color:var(--red)}
.alert.warn{background:var(--yellow-bg);border:1px solid rgba(210,153,34,.25);color:var(--yellow)}
.banner{border-radius:var(--radius);padding:12px 16px;margin:0 0 14px;font-size:13px;line-height:1.5}
.banner.dry{background:var(--yellow-bg);border:1px solid rgba(210,153,34,.25);color:var(--yellow)}
.banner.on{background:var(--green-bg);border:1px solid rgba(63,185,80,.25);color:var(--green)}
.e-chips{display:inline-flex;gap:6px;margin-left:8px;flex-wrap:wrap;vertical-align:middle}
.chip.wait{background:var(--yellow-bg);color:var(--yellow);border:1px solid rgba(210,153,34,.2)}
.plan-a{color:var(--blue);font-size:12px;margin-right:8px}
.now{position:relative}
.now-top{display:flex;align-items:center;gap:12px;flex-wrap:wrap;margin-bottom:12px}
.pill{font-size:12px;font-weight:600;letter-spacing:.3px;padding:6px 14px;border-radius:16px;flex-shrink:0}
.pill.run{background:var(--yellow-bg);color:var(--yellow);border:1px solid rgba(210,153,34,.25)}
.pill.wait{background:var(--yellow-bg);color:var(--yellow);border:1px solid rgba(210,153,34,.25)}
.pill.ok{background:var(--green-bg);color:var(--green);border:1px solid rgba(63,185,80,.25)}
.pill.roll{background:var(--blue-bg);color:var(--blue);border:1px solid rgba(88,166,255,.25)}
.pill.bad{background:var(--red-bg);color:var(--red);border:1px solid rgba(248,81,73,.25)}
.pill.warn{background:var(--yellow-bg);color:var(--yellow);border:1px solid rgba(210,153,34,.25)}
.now-line{font-size:14px;color:#e6eaf1;flex:1;min-width:280px}
.pipe{display:flex;align-items:center;gap:0;margin:6px 0 14px;flex-wrap:wrap}
.step{display:flex;align-items:center;gap:6px;color:var(--text2);font-size:11px;font-weight:600;text-transform:uppercase;letter-spacing:.5px;padding:4px 6px}
.step .dot{width:8px;height:8px;border-radius:50%;background:var(--surface2);border:2px solid var(--border2)}
.step.done{color:var(--green)}.step.done .dot{background:var(--green);border-color:var(--green)}
.step.cur{color:var(--yellow)}.step.cur .dot{background:var(--yellow);border-color:var(--yellow);box-shadow:0 0 0 3px rgba(210,153,34,.15);animation:pulse 1.6s infinite}
.step:not(:last-child)::after{content:"";width:20px;height:1px;background:var(--border);margin:0 6px}
.step.done:not(:last-child)::after{background:var(--green)}
.np-head{font-size:12px;font-weight:600;color:var(--blue);text-transform:uppercase;letter-spacing:.5px;margin:16px 0 8px;padding-top:12px;border-top:1px solid var(--border)}
.np-head:first-child{border-top:0;padding-top:0;margin-top:0}
.radio-search{display:flex;gap:8px;margin:10px 0;flex-wrap:wrap}
.radio-search input[type=text]{flex:1;min-width:160px;background:var(--bg);border:1px solid var(--border);border-radius:var(--radius);padding:8px 12px;color:#e6eaf1;font-size:13px}
.radio-search select,.radio-search button{background:var(--bg);border:1px solid var(--border);border-radius:var(--radius);padding:8px 12px;color:var(--text);font-size:13px;cursor:pointer}
.radio-search button:hover,.radio-add button:hover{background:var(--surface2)}
.radio-add button{background:var(--green-bg);border:1px solid rgba(63,185,80,.25);border-radius:var(--radius);padding:4px 12px;color:var(--green);font-size:12px;cursor:pointer;font-weight:500}
.radio-actions{margin-top:10px;font-size:12px;color:var(--text2)}
.np{display:flex;align-items:center;gap:8px;padding:6px 0;font-size:13px;color:var(--text)}
.np-dot{width:6px;height:6px;border-radius:50%;background:var(--green);animation:pulse 1.2s infinite;flex-shrink:0}
.np-a{color:var(--blue);font-weight:600}
.np-pos{margin-left:auto;flex-shrink:0;color:var(--text2)}
.tr{display:flex;align-items:center;gap:10px;padding:5px 0;font-size:13px;color:var(--text)}
.tr-stars{color:var(--yellow);font-size:12px;min-width:120px;flex-shrink:0}
.tr-name{color:#e6eaf1;flex:1;word-break:break-word}
.actlist{max-height:520px;overflow:auto;padding-right:6px}
.act{background:var(--surface2);border:1px solid var(--border);border-radius:var(--radius);padding:10px 14px;margin-top:8px}
.act-top{display:flex;align-items:center;gap:8px;flex-wrap:wrap;font-size:13px}
.act-top .tag{font-size:11px}
.act-target{font-size:12px;margin-top:4px;color:var(--text2)}
.now-file{color:var(--blue);font-family:"SFMono-Regular",Consolas,monospace;font-size:12px;word-break:break-all}
.last-action{margin:4px 0 12px;padding:8px 12px;background:var(--blue-bg);border:1px solid rgba(88,166,255,.2);border-radius:var(--radius);color:var(--blue);font-size:12px;word-break:break-word}
.last-action b{color:#e6eaf1}
.ralist{max-height:260px;overflow:auto}
.ra{display:flex;align-items:center;gap:8px;padding:5px 0;border-bottom:1px solid var(--border);font-size:13px;color:var(--text)}
.ra-text{flex:1;word-break:break-word}
.ra .chip.ok{background:var(--green-bg);color:var(--green);border:1px solid rgba(63,185,80,.25)}
.e{background:var(--surface);border:1px solid var(--border);border-radius:var(--radius);padding:12px 16px;margin-bottom:10px}
.e.issue{border-left:3px solid var(--red)}
.chip{display:inline-block;margin-left:8px;background:var(--red-bg);border:1px solid rgba(248,81,73,.25);color:var(--red);border-radius:12px;padding:2px 8px;font-size:11px;font-weight:600}
.e .ts{color:var(--text2);font-size:12px}.e .m{background:var(--blue-bg);color:var(--blue);border:1px solid rgba(88,166,255,.2);border-radius:4px;padding:2px 6px;font-size:11px}
.e .p{color:var(--text);font-size:12px;margin-left:6px}.e .sum{color:var(--green);font-size:13px;margin:6px 0 4px}
details summary{cursor:pointer;color:var(--text2);font-size:12px}
pre{white-space:pre-wrap;word-break:break-word;background:var(--bg);border:1px solid var(--border);border-radius:var(--radius);padding:10px;font:12px/1.5 "SFMono-Regular",Consolas,monospace;color:var(--text);max-height:340px;overflow:auto;margin:8px 0 0}
a{color:var(--accent);text-decoration:none}
a:hover{text-decoration:underline}
.journal{font-size:12px;color:var(--text2);padding:6px 0;border-top:1px solid var(--border);margin-top:8px}
.footer-art{text-align:center;margin-top:24px}
.footer-art img{width:100%;max-width:700px;height:auto;border-radius:8px}
footer{color:var(--text2);font-size:11px;text-align:center;margin-top:12px;letter-spacing:.3px}
/* Mobile: single column, larger tap targets, stacked layout */
.mobile-bar{display:none}
@media (max-width: 640px){
  .wrap{padding:16px 12px 40px}
  h1{font-size:18px}
  .sub{font-size:11px}
  .card{padding:14px 14px}
  details.collapse{padding:10px 12px}
  .integrations{grid-template-columns:1fr}
  .sc-stats{gap:4px 10px}
  table{display:block;overflow-x:auto;white-space:nowrap;-webkit-overflow-scrolling:touch}
  .radio-search{flex-direction:column}
  .radio-search input,.radio-search select,.radio-search button{width:100%}
  .now-top{flex-direction:column;align-items:flex-start;gap:8px}
  .now-line{min-width:0;font-size:13px}
  .pipe{gap:2px}
  .step{font-size:10px;padding:2px 4px}
  .step:not(:last-child)::after{width:12px;margin:0 4px}
  .actlist,.ralist{max-height:300px}
  button{padding:8px 14px;min-height:36px}
  .footer-art img{max-width:100%}
  .mobile-bar{display:flex;position:sticky;top:0;z-index:10;background:var(--bg);border-bottom:1px solid var(--border);gap:6px;padding:8px 0 10px;margin:0 -12px 12px;overflow-x:auto;white-space:nowrap;-webkit-overflow-scrolling:touch}
  .mobile-bar a{flex-shrink:0;background:var(--surface);border:1px solid var(--border);border-radius:16px;padding:6px 12px;font-size:12px;color:var(--text);text-decoration:none}
  .mobile-bar a:active{background:var(--surface2)}
}
</style></head><body><div class="wrap">
<header>
<div style="text-align:center;margin-bottom:16px"><img src="https://raw.githubusercontent.com/Lunatixz/nd-organizer/main/images/banner.png" alt="nd-organizer" style="max-width:100%;height:auto;border-radius:8px;opacity:.9"></div>
<h1><img src="https://raw.githubusercontent.com/Lunatixz/nd-organizer/main/images/icon.png" alt="nd-organizer" style="height:24px;width:24px;border-radius:4px">nd-organizer</h1>
<div class="sub">__COUNT__ events &middot; plugin: __PLUGIN__ &middot; mode: <b>__MODE__</b> &middot; checked __UPDATED__ &middot; auto-refresh 30s &middot; log: __LOG__</div>
</header>
<nav class="mobile-bar" id="mobileBar"><a href="#health">Health</a><a href="#activity">Activity</a><a href="#playback">Playback</a><a href="#radio">Radio</a><a href="#playlists">Playlists</a><a href="#actions">Actions</a><a href="#sidecars">Sidecars</a></nav>
__BANNER__
<div style="text-align:right;margin:8px 0"><button onclick="forceRescan()" style="background:var(--surface);border:1px solid var(--border);border-radius:var(--radius);padding:6px 14px;color:var(--accent);cursor:pointer;font-size:12px;font-weight:600">Force Rescan</button></div>
<details class="collapse" open id="health"><summary>Health &amp; integrations</summary><div class="collapse-body">__INTEGRATIONS__</div></details>
<details class="collapse" open id="activity"><summary>Current activity</summary><div class="collapse-body">__NOW__</div></details>
<details class="collapse" open id="playback"><summary>Playback</summary><div class="collapse-body">__PLAYBACK__</div></details>
<details class="collapse" open id="radio"><summary>Internet radio</summary><div class="collapse-body">__RADIO__</div></details>
<details class="collapse" open id="playlists"><summary>Smart playlists</summary><div class="collapse-body">__PLAYLISTS__</div></details>
<details class="collapse" open id="actions"><summary>Planned actions</summary><div class="collapse-body">__ALBUMS__</div></details>
<details class="collapse" id="tasks"><summary>Task queue</summary><div class="collapse-body">__TASKS__</div></details>
<details class="collapse" id="sidecars"><summary>Sidecars</summary><div class="collapse-body">__SIDECARS__</div></details>
<details class="collapse" id="recent"><summary>Recent actions</summary><div class="collapse-body">__RECENT__</div></details>
<details class="collapse" id="reports"><summary>Activity &amp; reports</summary><div class="collapse-body">__ROWS__</div></details>
<div class="footer-art"><img src="https://raw.githubusercontent.com/Lunatixz/nd-organizer/main/images/footer.png" alt="" style="width:100%;max-width:700px;height:auto;border-radius:8px"></div>
<footer>nd-organizer webhook dashboard</footer>
</div>
<script>
function forceRescan(){
  var btn=document.querySelector('[onclick="forceRescan()"]');
  if(btn){btn.disabled=true;btn.textContent='Rescanning...';}
  fetch('/force-rescan',{method:'POST'}).then(function(r){return r.json()}).then(function(d){
    if(btn){btn.disabled=false;btn.textContent='Force Rescan';}
    if(d.ok){alert('Rescan signal posted — next scheduled run will re-scan from scratch');}
    else{alert('Error: '+(d.error||'unknown'));}
  }).catch(function(){if(btn){btn.disabled=false;btn.textContent='Force Rescan';}alert('Request failed');});
}
// Persist collapsible-section open state across reloads.
(function () {
    var KEY = "ndorg.collapse.";
    document.querySelectorAll("details.collapse").forEach(function (d) {
        d.addEventListener("toggle", function () {
            var k = KEY + d.querySelector("summary").textContent.trim();
            try { localStorage.setItem(k, d.open ? "1" : "0"); } catch (e) {}
        });
    });
    document.querySelectorAll("details.collapse").forEach(function (d) {
        var k = KEY + d.querySelector("summary").textContent.trim();
        var v = null;
        try { v = localStorage.getItem(k); } catch (e) {}
        if (v === "1") d.open = true;
        if (v === "0") d.open = false;
    });
})();
// Silent refresh: every 30s swap in the new content WITHOUT reloading the page,
// so open/closed sections stay open and the page never flashes or jumps.
// Pauses for 60s after any user interaction (click, scroll, type).
(function () {
    var paused = false;
    var pauseTimer = null;
    var scrollTimer = null;
    function pauseRefresh() {
        paused = true;
        clearTimeout(pauseTimer);
        pauseTimer = setTimeout(function () { paused = false; }, 60000);
    }
    function throttledScroll() {
        if (scrollTimer) return;
        scrollTimer = setTimeout(function () { scrollTimer = null; pauseRefresh(); }, 200);
    }
    // Pause on meaningful user interaction
    document.addEventListener("click", pauseRefresh);
    document.addEventListener("keydown", pauseRefresh);
    document.addEventListener("scroll", throttledScroll, {passive: true});
    document.addEventListener("focusin", pauseRefresh);
    function refresh() {
        if (paused) return;
        var a = document.activeElement;
        if (a && (a.tagName === "INPUT" || a.tagName === "TEXTAREA" || a.tagName === "SELECT")) return;
        fetch(location.href, { headers: { Accept: "text/html" } })
            .then(function (r) { return r.text(); })
            .then(function (html) {
                var doc = new DOMParser().parseFromString(html, "text/html");
                var open = {};
                document.querySelectorAll("details.collapse").forEach(function (d) {
                    open[d.querySelector("summary").textContent.trim()] = d.open;
                });
                var fresh = doc.querySelector(".wrap");
                if (!fresh) return;
                var cur = document.querySelector(".wrap");
                cur.innerHTML = fresh.innerHTML;
                document.querySelectorAll("details.collapse").forEach(function (d) {
                    var k = d.querySelector("summary").textContent.trim();
                    if (k in open) d.open = open[k];
                });
            })
            .catch(function () {});
    }
    setInterval(refresh, 30000);
})();
</script>
</body></html>"""


class Server(http.server.ThreadingHTTPServer):
    daemon_threads = True


if __name__ == "__main__":
    load_log()
    log.info("=" * 60)
    log.info("nd-organizer-webhook starting")
    log.info("listening on 0.0.0.0:%d", PORT)
    log.info("log file: %s", LOGFILE)
    log.info("reloaded %d prior events from log", len(entries))
    log.info("waiting for the Navidrome plugin to POST reports/status to this URL")
    log.info("integrations panel is driven by the plugin's status JSON (no keys stored here)")
    log.info("=" * 60)
    Server(("0.0.0.0", PORT), Handler).serve_forever()
