#!/usr/bin/env python3
"""Real-browser check of the dashboard against a REAL server + daemon.

Boots `cairn server` + `cairn daemon`, attaches a project with 1,200 files,
opens the console in headless Chrome and asserts the review-backlog fixes
that only a browser can prove:

  #34/#35  files are paged (500 rows, "Show more" grows to 1,000) instead of
           the old silent 300-row cut
  #83      arrow keys move between file rows
  #75      the help overlay traps focus and hands it back on Escape
  #73/#94  a failed action shows a plain sentence with a support ref, never
           the raw backend string
  #57      a stale dashboard token reloads the page once
  hygiene  zero uncaught page errors

Usage: BIN=target/debug/cairn python3 scripts/ui_browser_check.py
Needs: python3 playwright + a Chrome/Chromium (CHROME= to override).
"""
import json
import os
import shutil
import socket
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

from playwright.sync_api import sync_playwright

REPO = Path(__file__).resolve().parent.parent
BIN = os.environ.get("BIN", str(REPO / "target/debug/cairn"))
WORK = Path(os.environ.get("WORK", REPO / ".ui-browser-check"))
SRV, OBJ = "127.0.0.1:7453", "127.0.0.1:7454"
CTL, UI = "127.0.0.1:17787", "127.0.0.1:17788"
N_FILES = 1200
FAILS = []


def check(ok, name, detail=""):
    print(f"[ui-check] {'PASS' if ok else 'FAIL'} {name}{'' if ok else f' ({detail})'}")
    if not ok:
        FAILS.append(name)


def wait_port(port, secs=30):
    end = time.time() + secs
    while time.time() < end:
        with socket.socket() as s:
            s.settimeout(0.4)
            if s.connect_ex(("127.0.0.1", port)) == 0:
                return True
        time.sleep(0.25)
    return False


def api(path, token, body=None):
    req = urllib.request.Request(
        f"http://{UI}{path}",
        data=None if body is None else json.dumps(body).encode(),
        headers={"x-cairn-token": token, "Content-Type": "application/json"},
        method="GET" if body is None else "POST",
    )
    with urllib.request.urlopen(req, timeout=15) as r:
        return json.loads(r.read())


def main():
    shutil.rmtree(WORK, ignore_errors=True)
    for port in (7453, 17787, 17788):
        if wait_port(port, 0.5):
            print(f"[ui-check] port {port} already in use - stop the old process first")
            return 2
    srv_home, home, root = WORK / "server", WORK / "dev", WORK / "root"
    root2 = WORK / "second"
    for d in (srv_home, home, root / "clips", root2):
        d.mkdir(parents=True)
    for i in range(3):
        (root2 / f"note{i}.txt").write_text("second project\n")
    for i in range(N_FILES):
        (root / "clips" / f"shot{i:04d}.txt").write_text(f"take {i}\n")

    env = {**os.environ, "CAIRN_HOME": str(home)}
    procs = []
    try:
        procs.append(subprocess.Popen(
            [BIN, "server", "--data-dir", str(srv_home), "--grpc-addr", SRV,
             "--objects-addr", OBJ, "--dev-insecure"],
            stdout=open(WORK / "server.log", "w"), stderr=subprocess.STDOUT))
        assert wait_port(7453), "server did not listen"

        def start_daemon():
            p = subprocess.Popen([BIN, "daemon", "--ctl-addr", CTL, "--ui-addr", UI], env=env,
                                 stdout=open(WORK / "daemon.log", "a"), stderr=subprocess.STDOUT)
            procs.append(p)  # always reaped in `finally`, even on a crash
            assert wait_port(17787) and wait_port(17788), "daemon did not listen"
            return p

        daemon = start_daemon()
        code = subprocess.check_output([BIN, "dev-enroll-code", "--server", SRV], env=env, text=True).strip()
        subprocess.check_call([BIN, "login", "--server", SRV, "--code", code, "--name", "ui-check"],
                              env=env, stdout=subprocess.DEVNULL)

        html = urllib.request.urlopen(f"http://{UI}/", timeout=10).read().decode()
        token = html.split('window.CAIRN_TOKEN = "')[1].split('"')[0]
        r = api("/api/v1/attach", token, {"root_path": str(root)})
        assert r.get("ok"), r
        project = r["project_id"]
        r2 = api("/api/v1/attach", token, {"root_path": str(root2)})
        assert r2.get("ok"), r2
        project2 = r2["project_id"]
        end = time.time() + 180
        while time.time() < end:
            page = api(f"/api/v1/files?project={project}&limit=1", token)
            if page.get("total", 0) >= N_FILES:
                break
            time.sleep(1)
        check(page.get("total", 0) >= N_FILES, "daemon indexed all files", page.get("total"))

        chrome = os.environ.get("CHROME") or shutil.which("google-chrome") or shutil.which("chromium")
        with sync_playwright() as pw:
            browser = pw.chromium.launch(executable_path=chrome, headless=True)
            # bypass_csp: the console's strict CSP (no eval) also blocks Playwright's
            # own wait_for_function predicates after a reload
            pg = browser.new_page(viewport={"width": 1400, "height": 900}, bypass_csp=True)
            errors = []
            pg.on("pageerror", lambda e: errors.append(str(e)))
            pg.goto(f"http://{UI}/")
            pg.click("#nav-files")
            pg.wait_for_function(f"[...document.querySelectorAll('#files-project option')].some(o => o.value === '{project}')", timeout=20000)
            pg.select_option("#files-project", project)
            pg.wait_for_selector("#files-body tr.file-row", timeout=20000)
            pg.wait_for_function("document.querySelectorAll('#files-body tr.file-row').length >= 500", timeout=20000)
            n = pg.locator("#files-body tr.file-row").count()
            check(n == 500, "first page renders 500 rows (was a silent 300 cut)", n)
            more_txt = pg.locator(".files-more").inner_text()
            check(f"{N_FILES:,}" in more_txt or str(N_FILES) in more_txt, "says 'Showing 500 of 1,200'", more_txt)
            pg.click(".files-more button")
            pg.wait_for_function("document.querySelectorAll('#files-body tr.file-row').length >= 1000", timeout=20000)
            check(pg.locator("#files-body tr.file-row").count() == 1000, "Show more grows to 1,000 rows")

            # #83 keyboard row navigation
            pg.locator("#files-body tr.file-row").first.focus()
            pg.keyboard.press("ArrowDown")
            pg.keyboard.press("ArrowDown")
            idx = pg.evaluate("[...document.querySelectorAll('#files-body tr.file-row')].indexOf(document.activeElement)")
            check(idx == 2, "ArrowDown moves focus between rows", idx)
            pg.keyboard.press("End")
            idx = pg.evaluate("[...document.querySelectorAll('#files-body tr.file-row')].indexOf(document.activeElement)")
            check(idx == 999, "End jumps to the last loaded row", idx)

            # #75 help overlay: focus trapped inside, returned on Escape
            pg.focus("#foot-help")
            pg.click("#foot-help")
            inside = pg.evaluate("document.getElementById('help-overlay').contains(document.activeElement)")
            check(inside, "help overlay takes focus")
            for _ in range(15):
                pg.keyboard.press("Tab")
            inside = pg.evaluate("document.getElementById('help-overlay').contains(document.activeElement)")
            check(inside, "Tab stays trapped in the help overlay")
            pg.keyboard.press("Escape")
            back = pg.evaluate("document.activeElement && document.activeElement.id")
            check(back == "foot-help", "Escape returns focus to the opener", back)

            # #73/#94 friendly errors with a support ref
            msg = pg.evaluate("""async () => {
                const r = await postJSON('/api/v1/snapshots/restore', {});
                return { raw: r.error, ref: r.request_id, shown: humanError(r) };
            }""")
            check(bool(msg["ref"]), "failed action carries a request_id", msg)
            check(msg["raw"] not in msg["shown"] and msg["ref"] in msg["shown"],
                  "user sees a plain sentence + ref, not the raw error", msg)
            logged = msg["ref"] in (WORK / "daemon.log").read_text(errors="replace")
            check(logged, "the same ref is in daemon.log")

            # #8/#10: one project context drives People too
            pg.click("#nav-people")
            pg.wait_for_selector("#people-project", state="visible", timeout=10000)
            pg.select_option("#people-project", project2)
            same = pg.evaluate("document.getElementById('files-project').value")
            check(same == project2, "People picker moves the shared project context", same)
            team = pg.evaluate("getJSON('/api/v1/team?project=' + encodeURIComponent(selectedProject()))")
            got = (team.get("projects") or [{}])[0].get("project_id")
            check(got == project2, "People shows the selected project, not the first one", got)

            check(not errors, "no uncaught page errors", errors[:3])

            # #57 stale token: restart the daemon behind the open page
            daemon.terminate()
            daemon.wait(10)
            # wait for the port to free so the restart really is a new process
            end = time.time() + 15
            while time.time() < end and wait_port(17788, 0.3):
                time.sleep(0.3)
            daemon = start_daemon()
            pg.evaluate("sessionStorage.removeItem('cairn.reloadAt')")
            with pg.expect_navigation(timeout=30000):
                pg.evaluate("getJSON('/api/v1/status').catch(() => {})")
            pg.wait_for_function("window.CAIRN_TOKEN && window.CAIRN_TOKEN.length === 32", timeout=15000)
            ok = pg.evaluate("getJSON('/api/v1/status').then(() => true, () => false)")
            check(ok, "page reloaded onto the new daemon's token and works")
            browser.close()
    finally:
        for p in procs:
            p.terminate()
    print(f"[ui-check] RESULT: {'PASS' if not FAILS else 'FAIL: ' + ', '.join(FAILS)}")
    return 1 if FAILS else 0


if __name__ == "__main__":
    sys.exit(main())
