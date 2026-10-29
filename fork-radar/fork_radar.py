"""Fork-Radar (eigene Fassung, jrmagin): sammelt, was andere Forks von tangency-labs/solidcraft
selbst geändert haben, für den wöchentlichen Bericht.

Aufruf:  python -I fork_radar.py
Braucht: gh (angemeldet), git, den Klon C:\\Projekte\\SolidCraft mit dem Remote `upstream`.
Schreibt nach %LOCALAPPDATA%\\SolidCraft-Radar\\:
  radar-JJJJ-MM-TT.json  alle Forks mit eigenen Commits; `neu` = seit dem letzten Lauf verändert
  state.json             je Fork der zuletzt gesehene Stand
Ändert nichts am Klon außer `git fetch upstream`.
"""
import datetime as dt
import json
import os
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor

UPSTREAM = "tangency-labs/solidcraft"
CLONE = r"C:\Projekte\SolidCraft"
OWN = "jrmagin"
OUT = os.path.join(os.environ.get("LOCALAPPDATA", "."), "SolidCraft-Radar")
MAX_TITLES = 40
MAX_FILES = 60


def run(args):
    r = subprocess.run(args, capture_output=True, text=True, encoding="utf-8", errors="replace")
    if r.returncode != 0:
        raise RuntimeError(f"{' '.join(args[:3])}: {r.stderr.strip()[:300]}")
    return r.stdout


def when(s):
    return dt.datetime.fromisoformat(s.replace("Z", "+00:00"))


def forks():
    out = run(["gh", "api", "--paginate", f"repos/{UPSTREAM}/forks?per_page=100&sort=newest",
               "--jq", ".[] | {name: .full_name, branch: .default_branch, created: .created_at, pushed: .pushed_at,"
                       " stars: .stargazers_count, description: (.description // \"\"), license: (.license.spdx_id // \"\")}"])
    return [json.loads(line) for line in out.splitlines() if line.strip()]


def upstream_titles():
    """Commit-Titel im Original, um schon übernommene Änderungen zu erkennen."""
    run(["git", "-C", CLONE, "fetch", "upstream", "--quiet"])
    return set(run(["git", "-C", CLONE, "log", "upstream/main", "--format=%s", "-n", "20000"]).splitlines())


def compare(fork):
    owner = fork["name"].split("/")[0]
    try:
        d = json.loads(run(["gh", "api", f"repos/{UPSTREAM}/compare/main...{owner}:{fork['branch']}"]))
    except (RuntimeError, json.JSONDecodeError) as e:
        return {**fork, "fehler": str(e)}
    own = [c for c in d.get("commits", []) if len(c.get("parents", [])) == 1]
    head = own[-1]["sha"] if own else (d.get("commits") or [{}])[-1].get("sha", "")
    return {
        **fork,
        "ahead": d.get("ahead_by", 0),
        "behind": d.get("behind_by", 0),
        "head": head,
        "titel": [{"datum": c["commit"]["author"]["date"][:10], "titel": c["commit"]["message"].splitlines()[0]} for c in own][-MAX_TITLES:],
        "dateien": [f"{f['status']} +{f['additions']}/-{f['deletions']} {f['filename']}" for f in d.get("files", [])][:MAX_FILES],
        "dateien_gesamt": len(d.get("files", [])),
    }


def main():
    os.makedirs(OUT, exist_ok=True)
    state_path = os.path.join(OUT, "state.json")
    try:
        state = json.load(open(state_path, encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        state = {}
    all_forks = forks()
    # Nur Forks, in die nach dem Anlegen noch etwas hochgeladen wurde.
    active = [f for f in all_forks if not f["name"].startswith(OWN + "/") and (when(f["pushed"]) - when(f["created"])).total_seconds() > 600]
    known = upstream_titles()
    with ThreadPoolExecutor(8) as pool:
        results = list(pool.map(compare, active))
    found = []
    for r in results:
        if r.get("ahead", 0) <= 0:
            continue
        for t in r["titel"]:
            t["im_original"] = t["titel"] in known
        r["neu"] = state.get(r["name"]) != r["head"]
        found.append(r)
    found.sort(key=lambda r: (not r["neu"], -r["ahead"]))
    today = dt.date.today().isoformat()
    report = {
        "datum": today,
        "forks_gesamt": len(all_forks),
        "aktiv": len(active),
        "mit_eigenen_commits": len(found),
        "neu_seit_letztem_lauf": sum(1 for r in found if r["neu"]),
        "fehler": [r["name"] for r in results if "fehler" in r],
        "forks": found,
    }
    path = os.path.join(OUT, f"radar-{today}.json")
    tmp = path + ".tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(report, f, ensure_ascii=False, indent=1)
    os.replace(tmp, path)
    new_state = {r["name"]: r["head"] for r in found}
    with open(state_path + ".tmp", "w", encoding="utf-8") as f:
        json.dump(new_state, f, indent=1)
    os.replace(state_path + ".tmp", state_path)
    print(f"{path}: {report['forks_gesamt']} Forks, {report['aktiv']} aktiv, {report['mit_eigenen_commits']} mit eigenen Commits, "
          f"{report['neu_seit_letztem_lauf']} neu oder verändert, {len(report['fehler'])} Fehler")


if __name__ == "__main__":
    sys.exit(main())
