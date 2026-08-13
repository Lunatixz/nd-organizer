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
import sys
import time
from datetime import datetime, timezone

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s %(levelname)s [webhook] %(message)s",
    datefmt="%Y-%m-%d %H:%M:%S",
)
log = logging.getLogger("webhook")

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 8099
LOGFILE = sys.argv[2] if len(sys.argv) > 2 else "webhook.log"

entries = []  # list of (ts, path, body)
services = {}  # sidecar name -> last heartbeat unix ts
last_any_request = time.time()  # webhook's own liveness


def load_log():
    try:
        with open(LOGFILE, "r", encoding="utf-8", errors="replace") as f:
            for line in f:
                line = line.rstrip("\n")
                if line.startswith("[") and "] " in line:
                    ts, rest = line[1:].split("] ", 1)
                    path, body = rest.split(" - ", 1)
                    entries.append((ts, path, body))
    except FileNotFoundError:
        pass


def esc(s):
    return (str(s).replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;"))


# ---------------------------------------------------------------- integrations

def integrations_html():
    """Render the integrations panel from the plugin's status JSON (which
    contains the plugin's own connectivity + auth checks). No probing here, no
    keys. Includes an overall health summary and an alert banner when any
    service needs attention, plus the age of the last status report."""
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
    for it in found:
        if not isinstance(it, dict):
            continue
        name = it.get("name", "?")
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

    cards += service_cards()
    return banner + summary + "<div class='integrations'>%s</div>" % cards


def service_cards():
    """Sidecar liveness cards (from heartbeats + the webhook's own last
    request). Green = seen recently, red = stale/no signal."""
    now = time.time()
    services["webhook"] = last_any_request
    cards = ""
    for name in sorted(services):
        age = max(0, int(now - services[name]))
        if age < 120:
            cls, label = "ok", "UP"
        elif age < 600:
            cls, label = "warn", "WEAK"
        else:
            cls, label = "bad", "STALE"
        cards += ("<div class='ig'><div class='ig-top'><span class='ig-name'>%s</span>"
                  "<span class='ig-state %s'>%s</span></div>"
                  "<span class='dim'>last signal %ds ago</span></div>") % (
            esc(name), cls, label, age)
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
    for t in found:
        if not isinstance(t, dict):
            continue
        st = t.get("state", "?")
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
    return rows


# ---------------------------------------------------------------- dashboard bits

def status_card(body):
    try:
        j = json.loads(body)
        if not isinstance(j, dict) or "mode" not in j:
            return None
    except Exception:
        return None
    if j.get("inProgress"):
        if j.get("phase") == "scan":
            tag = "<span class='tag run'>SCANNING</span>"
        else:
            tag = "<span class='tag run'>RUNNING</span>"
    elif j.get("deferredUntilIdle"):
        tag = "<span class='tag wait'>WAITING FOR IDLE</span>"
    else:
        tag = "<span class='tag ok'>IDLE</span>"
    scan_line = ""
    if j.get("phase") == "scan":
        scan_line = "<div class='note'>Scanning library... <b>%s</b> files indexed so far (chunk of %s).</div>" % (
            int(j.get("filesScanned", 0)), int(j.get("chunkSize", 0)))
    batch = ""
    b = j.get("batch")
    if isinstance(b, dict) and b.get("total"):
        batch = "<span class='tag'>batch %d/%d</span>" % (int(b.get("index", 0)) + 1, int(b["total"]))
    ts = ""
    if j.get("ts"):
        try:
            ts = datetime.fromtimestamp(int(j["ts"]), tz=timezone.utc).strftime("%H:%M:%S UTC")
        except Exception:
            pass
    html = "<div class='card'><h2>Status <span class='meta'>%s</span></h2>" % ts
    html += "<div class='kv'>%s <span class='tag mode'>%s</span> %s" % (tag, esc(j.get("mode", "")), batch)
    html += scan_line
    if j.get("runId"):
        html += " <span class='tag'>run %s</span>" % esc(j["runId"])
        html += ("<div class='rollback'>Want to undo this run? Set <b>rollbackRunId</b> = "
                 "<code>%s</code> in the plugin settings, then run a pass. Files, folders and "
                 "album.nfo are restored from backup.</div>") % esc(j["runId"])
    if j.get("rollbackOfRun"):
        html += " <span class='tag'>rollback of %s</span>" % esc(j["rollbackOfRun"])
    html += "</div>"
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
        plans = j.get("plans")
        if isinstance(plans, list) and plans:
            html += "<div class='plans'><b>Album plans in this batch:</b>"
            kind_label = {"soundtrack": "Soundtrack", "various": "Various", "singles": "Single/Incomplete", "normal": "Album"}
            for p in plans:
                if not isinstance(p, dict):
                    continue
                kind = p.get("kind", "normal")
                html += ("<div class='plan'><span class='plan-k'>%s</span> <span class='plan-t'>/%s</span>"
                         "<span class='dim'>%d dupes, %d filler</span>") % (
                    esc(kind_label.get(kind, kind)), esc(p.get("target", "")),
                    int(p.get("duplicates", 0)), int(p.get("fillers", 0)))
                moves = p.get("moves")
                if isinstance(moves, list) and moves:
                    html += "<div class='moves'>"
                    for mv in moves:
                        if isinstance(mv, dict):
                            html += "<div class='move'><span class='mv-f'>%s</span> &#8594; <span class='mv-t'>%s</span></div>" % (
                                esc(mv.get("from", "")), esc(mv.get("to", "")))
                    html += "</div>"
                html += "</div>"
            html += "</div>"
    elif j.get("phase") == "stats":
        html += ("<div class='note'>Stats heartbeat: <b>%s</b> plays, <b>%s</b> skips, "
                 "<b>%s</b> top picks, <b>%s</b> flagged to the filter proxy.</div>") % (
            int(j.get("plays", 0)), int(j.get("skips", 0)),
            int(j.get("topPicks", 0)), int(j.get("filtered", 0)))
    elif j.get("deferredUntilIdle"):
        html += "<div class='note'>Run was deferred because playback is active. It retries automatically.</div>"
    else:
        html += "<div class='note'>No libraries processed yet.</div>"
    warns = j.get("warnings")
    if isinstance(warns, list) and warns:
        html += "<div class='warn'><b>Warnings:</b><ul>"
        for w in warns:
            html += "<li>%s</li>" % esc(w)
        html += "</ul></div>"
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
            self.wfile.write(b"ok")
            return
        summary = entry_summary(body) or "report/log"
        log.info("received POST %s from %s (%d bytes) - %s", self.path, self.client_address[0], len(body), summary)
        entries.append((ts, self.path, body))
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
        self.wfile.write(b"ok\n")

    def do_GET(self):
        global last_any_request
        last_any_request = time.time()
        if self.path.startswith("/health"):
            data = json.dumps({
                "ok": True, "service": "nd-organizer-webhook", "port": PORT,
                "events": len(entries), "log": LOGFILE,
            }).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
            return

        card = None
        for _, _, body in reversed(entries):
            card = status_card(body)
            if card:
                break
        card = card or ""
        rows = ""
        for ts, path, body in reversed(entries):
            summary = entry_summary(body)
            issue = None
            try:
                j = json.loads(body)
                if isinstance(j, dict):
                    bad = [i.get("name", "?") for i in (j.get("integrations") or [])
                           if i.get("state") in ("unreachable", "authFailed")]
                    warns = j.get("warnings") or []
                    if bad:
                        issue = "ISSUES: " + ", ".join(map(str, bad))
                    elif warns:
                        issue = "WARNINGS: %d" % len(warns)
            except Exception:
                pass
            cls = " class='e issue'" if issue else " class='e'"
            rows += ("<div%s><span class='ts'>%s</span> <span class='m'>POST</span> <span class='p'>%s</span>" % (cls, ts, esc(path)))
            if issue:
                rows += "<span class='chip issue'>%s</span>" % esc(issue)
            if summary:
                rows += "<div class='sum'>%s</div>" % esc(summary)
                rows += "<details><summary>raw json</summary><pre>%s</pre></details>" % esc(body)
            else:
                rows += "<details open><summary>report / log</summary><pre>%s</pre></details>" % esc(body)
            rows += "</div>"
        if not rows:
            rows = "<div class='note'>Waiting for the plugin to POST its status/reports &hellip;</div>"

        plugin_state = "connected" if entries else "no activity yet"
        updated = datetime.now().strftime("%H:%M:%S")

        page = (PAGE
                .replace("__COUNT__", str(len(entries)))
                .replace("__PLUGIN__", plugin_state)
                .replace("__UPDATED__", updated)
                .replace("__LOG__", esc(LOGFILE))
                .replace("__INTEGRATIONS__", integrations_html())
                .replace("__CARD__", card)
                .replace("__TASKS__", tasks_html())
                .replace("__ROWS__", rows))
        data = page.encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, *a):
        pass


PAGE = """<!DOCTYPE html><html><head><meta charset="utf-8"><meta http-equiv="refresh" content="5">
<title>nd-organizer</title>
<style>
:root{color-scheme:dark}
*{box-sizing:border-box}
body{background:linear-gradient(180deg,#0d1117 0%,#0a0e14 100%);color:#d7dde6;font:14px/1.55 -apple-system,"Segoe UI",Roboto,Helvetica,Arial,sans-serif;margin:0;min-height:100vh}
.wrap{max-width:1060px;margin:0 auto;padding:24px 20px 60px}
header{margin-bottom:20px}
h1{font-size:22px;margin:0;color:#8ab4f8;display:flex;align-items:center;gap:10px}
h1 .dot{width:10px;height:10px;border-radius:50%;background:#3fb950;display:inline-block;animation:pulse 2s infinite}
@keyframes pulse{0%,100%{opacity:1}50%{opacity:.35}}
.sub{color:#8b93a5;font-size:12px;margin-top:4px}
.badges{display:flex;gap:8px;margin-top:8px;flex-wrap:wrap}
.badge{background:#161b24;border:1px solid #232a36;border-radius:20px;padding:3px 12px;font-size:12px;color:#c8d0db}
.badge b{color:#e6eaf1}
.card{background:#141a24;border:1px solid #232a36;border-radius:12px;padding:16px 18px;margin-bottom:20px;box-shadow:0 1px 0 rgba(255,255,255,.03)}
details.collapse{background:#141a24;border:1px solid #232a36;border-radius:12px;margin-bottom:14px;padding:13px 16px;box-shadow:0 1px 0 rgba(255,255,255,.03)}
details.collapse summary{list-style:none;cursor:pointer;font-size:15px;color:#e6eaf1;letter-spacing:.2px;font-weight:600;margin:0;padding:1px 0}
details.collapse summary::before{content:"[+] ";color:#8b93a5;font-size:13px;font-weight:bold}
details.collapse[open] summary::before{content:"[-] "}
.collapse-body{margin-top:12px}
h2{font-size:15px;margin:0 0 12px;color:#e6eaf1;letter-spacing:.2px}
h2 .meta{color:#8b93a5;font-size:12px;font-weight:normal;float:right}
.kv{display:flex;gap:8px;flex-wrap:wrap;align-items:center;margin-bottom:10px}
.tag{background:#232a36;border-radius:12px;padding:2px 10px;font-size:12px;color:#c8d0db}
.tag.run{background:#6b3a00;color:#ffcf8a}.tag.wait{background:#3a2c00;color:#ffd98a}
.tag.ok{background:#0f3d24;color:#8ff0b5}.tag.bad{background:#7a1b1b;color:#ff8f8f}
.tag.mode{background:#1d3a5f;color:#9cc8ff}
.rollback{margin-top:10px;background:#1d2a4a;border:1px solid #3a5a9c;border-radius:8px;padding:8px 12px;color:#aac8ff;font-size:13px}
.rollback code{background:#0a1220;border:1px solid #2c3e66;border-radius:4px;padding:1px 6px;color:#e6f0ff}
.plans{margin-top:10px;font-size:13px}
.plan{background:#161d29;border:1px solid #232a36;border-radius:8px;padding:8px 12px;margin-top:8px}
.plan-k{display:inline-block;background:#1d3a5f;color:#9cc8ff;border-radius:4px;padding:1px 8px;font-size:11px;font-weight:600;margin-right:8px}
.plan-t{font-weight:600;color:#e6eaf1;margin-right:10px}
.moves{margin-top:6px;font-size:12px}
.move{padding:1px 0;color:#aab6c5}
.mv-f{color:#ff9b9b}
.mv-t{color:#8ff0b5}
.tk{display:flex;align-items:center;gap:8px;padding:6px 0;border-bottom:1px solid #1c2230;font-size:13px}
.tk-ts{color:#8b93a5;font-size:12px;width:64px;flex-shrink:0}
.tk-kind{font-weight:600;color:#e6eaf1;min-width:70px}
.tk-msg{color:#9be0a6;flex:1;word-break:break-word}
table{width:100%;border-collapse:collapse;font-size:13px;margin:6px 0}
th{text-align:left;color:#8b93a5;font-weight:500;padding:4px 8px;border-bottom:1px solid #232a36;font-size:12px}
td{padding:4px 8px;border-bottom:1px solid #1c2230}
.dim{color:#8b93a5;font-size:12px}.totals{margin-top:8px;color:#c8d0db}
.note{color:#8b93a5;font-size:13px}
.warn{background:#2a1f12;border:1px solid #5c4a1e;border-radius:8px;padding:8px 12px;margin-top:10px;color:#ffd9a0;font-size:13px}
.warn ul{margin:6px 0 0;padding-left:18px}
.integrations{display:grid;grid-template-columns:repeat(auto-fill,minmax(200px,1fr));gap:10px}
.ig{background:#141a24;border:1px solid #232a36;border-radius:10px;padding:10px 12px}
.ig-top{display:flex;justify-content:space-between;align-items:center;gap:6px;margin-bottom:6px}
.ig-name{font-weight:600;color:#e6eaf1;font-size:13px}
.ig-state{font-size:11px;font-weight:600;padding:2px 8px;border-radius:10px}
.ig-state.ok{background:#0f3d24;color:#8ff0b5}
.ig-state.warn{background:#3a2c00;color:#ffd98a}
.ig-state.bad{background:#4b1010;color:#ff9b9b}
.ig-state.authfail{background:#7a1b1b;color:#ff8f8f}
.ig-state.dim{background:#232a36;color:#8b93a5}
.ig .dim{font-size:11px;word-break:break-all}
.ig-sum{margin:0 0 10px;color:#c8d0db;font-size:13px}
.alert{border-radius:10px;padding:10px 14px;margin:0 0 14px;font-size:13px;line-height:1.5}
.alert.bad{background:#2a1010;border:1px solid #6b2020;color:#ffb0b0}
.alert.warn{background:#2a2208;border:1px solid #6b5a1e;color:#ffd98a}
.e{background:#141a24;border:1px solid #232a36;border-radius:10px;padding:10px 14px;margin-bottom:10px}
.e.issue{border-left:3px solid #a12626}
.chip{display:inline-block;margin-left:8px;background:#2a1010;border:1px solid #6b2020;color:#ffb0b0;border-radius:10px;padding:1px 8px;font-size:11px;font-weight:600}
.e .ts{color:#8b93a5;font-size:12px}.e .m{background:#1d3a5f;color:#9cc8ff;border-radius:4px;padding:1px 6px;font-size:11px}
.e .p{color:#aab6c5;font-size:12px;margin-left:6px}.e .sum{color:#9be0a6;font-size:13px;margin:6px 0 4px}
details summary{cursor:pointer;color:#8b93a5;font-size:12px}
pre{white-space:pre-wrap;word-break:break-word;background:#0a0e14;border:1px solid #1c2230;border-radius:8px;padding:8px;font:12px/1.45 "SFMono-Regular",Consolas,monospace;color:#c8d0db;max-height:340px;overflow:auto;margin:6px 0 0}
a{color:#8ab4f8;text-decoration:none}
footer{color:#5c6470;font-size:11px;text-align:center;margin-top:28px}
</style></head><body><div class="wrap">
<header>
<h1><span class="dot"></span>nd-organizer</h1>
<div class="sub">__COUNT__ events &middot; plugin: __PLUGIN__ &middot; checked __UPDATED__ &middot; auto-refresh 5s &middot; log: __LOG__</div>
</header>
<details class="collapse" open><summary>Integrations</summary><div class="collapse-body">__INTEGRATIONS__</div></details>
<details class="collapse" open><summary>Status</summary><div class="collapse-body">__CARD__</div></details>
<details class="collapse"><summary>Task queue</summary><div class="collapse-body">__TASKS__</div></details>
<details class="collapse"><summary>Activity</summary><div class="collapse-body">__ROWS__</div></details>
<footer>nd-organizer webhook dashboard</footer>
</div>
<script>
// Persist collapsible-section open state across the 5s auto-refresh.
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
