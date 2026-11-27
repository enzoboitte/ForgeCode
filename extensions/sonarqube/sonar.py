#!/usr/bin/env python3
"""SonarQube pour ForgeCode : analyse, problèmes, règles, serveur local Docker.

Usage : sonar <analyze|issues [--file]|fix|rule|local-start|local-stop|configure|dashboard|status>

Configuration du projet (créée par `configure` ou `local-start`) : .forgecode/sonarqube.json,
ajouté au .gitignore de .forgecode. SONAR_HOST_URL et SONAR_TOKEN la remplacent s'ils sont définis.
Le mot de passe admin du serveur local (partagé par tous les projets) est dans local-server.json,
dans le dossier du module.
Seule la bibliothèque standard de Python 3.8+ est utilisée.
"""

import base64
import http.client
import json
import os
import platform
import re
import secrets
import shutil
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
import webbrowser
from pathlib import Path

for stream in (sys.stdout, sys.stderr):
    try:
        stream.reconfigure(encoding="utf-8", line_buffering=True)
    except AttributeError:
        pass

HERE = Path(__file__).resolve().parent
# Before 1.2 the configuration lived next to the module; still read once to migrate it.
LEGACY_CONFIG = HERE / "config.json"
# The local Docker server is shared by every project: its admin password stays with the module.
LOCAL_SERVER = HERE / "local-server.json"
LOCAL_URL = "http://localhost:9000"
COMPOSE_PROJECT = "forgecode-sonarqube"
STATES = {"DOWN": "pas encore joignable", "STARTING": "démarrage", "DB_MIGRATION_NEEDED": "migration de la base nécessaire",
          "DB_MIGRATION_RUNNING": "migration de la base", "UP": "prêt"}
SEVERITIES = ["INFO", "MINOR", "MAJOR", "CRITICAL", "BLOCKER"]
EXCLUSIONS = "**/node_modules/**,**/target/**,**/build/**,**/dist/**,**/vendor/**,**/.git/**,**/.scannerwork/**,**/__pycache__/**,**/.venv/**"


class Fail(Exception):
    pass


def forge(action, **fields):
    """Ask ForgeCode to do something (notification, open a file, chat…)."""
    print("@forge " + json.dumps({"action": action, **fields}, ensure_ascii=False))


def workspace():
    return Path(os.environ.get("FORGE_WORKSPACE") or os.getcwd()).resolve()


# ---------------------------------------------------------------- configuration

def config_path():
    """The project's configuration: `.forgecode/sonarqube.json` (kept out of git)."""
    return workspace() / ".forgecode" / "sonarqube.json"


def read_json(path):
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except ValueError as e:
        raise Fail(f"{path} n'est pas du JSON valide : {e}")


def load_config():
    path = config_path()
    cfg = read_json(path) if path.exists() else {}
    if not cfg and LEGACY_CONFIG.exists():
        cfg = read_json(LEGACY_CONFIG)
        cfg.pop("admin_password", None)
    if os.environ.get("SONAR_HOST_URL"):
        cfg["url"] = os.environ["SONAR_HOST_URL"]
    if os.environ.get("SONAR_TOKEN"):
        cfg["token"] = os.environ["SONAR_TOKEN"]
    cfg["url"] = (cfg.get("url") or "").rstrip("/")
    return cfg


def write_secret(path, data):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data, indent=2), encoding="utf-8")
    if os.name != "nt":
        os.chmod(path, 0o600)  # holds a token or a password


def save_config(cfg):
    path = config_path()
    write_secret(path, {k: v for k, v in cfg.items() if k != "admin_password"})
    # The token must never be committed.
    ignore = path.parent / ".gitignore"
    lines = ignore.read_text(encoding="utf-8").splitlines() if ignore.exists() else []
    if path.name not in lines:
        ignore.write_text("\n".join(lines + [path.name]) + "\n", encoding="utf-8")


def admin_password():
    if LOCAL_SERVER.exists():
        return read_json(LOCAL_SERVER).get("admin_password")
    if LEGACY_CONFIG.exists():
        return read_json(LEGACY_CONFIG).get("admin_password")
    return None


def require_server(cfg):
    if not cfg.get("url") or not cfg.get("token"):
        raise Fail("Aucun serveur configuré. Lancez « SonarQube : démarrer un serveur local (Docker) » "
                   "ou « SonarQube : se connecter à un serveur… ».")


def project_key(root):
    props = root / "sonar-project.properties"
    if props.exists():
        m = re.search(r"^\s*sonar\.projectKey\s*=\s*(\S+)", props.read_text(encoding="utf-8", errors="replace"), re.M)
        if m:
            return m.group(1)
    return re.sub(r"[^A-Za-z0-9_.:-]", "_", root.name) or "project"


# ---------------------------------------------------------------- HTTP API

def api(cfg, path, params=None, method="GET", auth=None):
    url = cfg["url"] + path
    data = None
    if params and method == "GET":
        url += "?" + urllib.parse.urlencode(params)
    elif params:
        data = urllib.parse.urlencode(params).encode()
    req = urllib.request.Request(url, data=data, method=method)
    user, password = auth or (cfg.get("token", ""), "")
    if user:
        req.add_header("Authorization", "Basic " + base64.b64encode(f"{user}:{password}".encode()).decode())
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            body = resp.read()
            return json.loads(body) if body.strip() else {}
    except urllib.error.HTTPError as e:
        detail = e.read().decode(errors="replace")
        try:
            detail = "; ".join(err["msg"] for err in json.loads(detail).get("errors", [])) or detail
        except ValueError:
            pass
        if e.code == 401:
            raise Fail(f"Authentification refusée par {cfg['url']} : vérifiez le jeton.")
        raise Fail(f"{method} {path} → HTTP {e.code} : {detail[:300]}")
    except urllib.error.URLError as e:
        raise Fail(f"Serveur injoignable ({cfg['url']}) : {e.reason}")
    # A server still starting accepts the connection then drops it (RemoteDisconnected,
    # connection reset, timeout) or answers with something that isn't JSON.
    except (http.client.HTTPException, OSError, ValueError) as e:
        raise Fail(f"Le serveur {cfg['url']} ne répond pas encore correctement ({type(e).__name__})")


def server_status(cfg):
    try:
        return api(cfg, "/api/system/status", auth=("", "")).get("status", "?")
    except Fail:
        return "DOWN"


# ---------------------------------------------------------------- local server (Docker)

def compose_cmd():
    if not shutil.which("docker"):
        raise Fail("Docker est introuvable. Installez Docker Desktop (macOS, Windows) ou Docker Engine (Linux).")
    if subprocess.run(["docker", "compose", "version"], capture_output=True).returncode == 0:
        return ["docker", "compose"]
    if shutil.which("docker-compose"):
        return ["docker-compose"]
    raise Fail("Docker Compose est introuvable (ni `docker compose` ni `docker-compose`).")


def compose(*args):
    cmd = compose_cmd() + ["-p", COMPOSE_PROJECT, "-f", str(HERE / "docker-compose.yml"), *args]
    print("$ " + " ".join(cmd))
    if subprocess.run(cmd).returncode != 0:
        raise Fail("docker compose a échoué (Docker est-il démarré ?).")


def local_start():
    compose("up", "-d")
    cfg = load_config()
    cfg.update(mode="local", url=LOCAL_URL)
    print("Démarrage de SonarQube (1 à 3 minutes la première fois)…")
    deadline = time.time() + 600
    last = None
    while True:
        status = server_status(cfg)
        if status != last:
            print(f"  état : {STATES.get(status, status)}")
            last = status
        if status == "UP":
            break
        if time.time() > deadline:
            raise Fail("SonarQube n'a pas démarré en 10 minutes : `docker compose -p forgecode-sonarqube logs sonarqube`.")
        time.sleep(5)
    if not cfg.get("token") or not token_valid(cfg):
        cfg["token"] = create_local_token(cfg)
    save_config(cfg)
    print(f"SonarQube local prêt : {LOCAL_URL} (utilisateur admin, mot de passe dans {LOCAL_SERVER})")
    print(f"Configuration du projet : {config_path()}")
    forge("notify", message="SonarQube local prêt")


def token_valid(cfg):
    try:
        return api(cfg, "/api/authentication/validate").get("valid", False)
    except Fail:
        return False


def create_local_token(cfg):
    """Replace the default admin/admin password, then create a user token for the scanner."""
    password = admin_password()
    if not password or not try_auth(cfg, ("admin", password)):
        if not try_auth(cfg, ("admin", "admin")):
            raise Fail("Impossible de se connecter en admin. Créez un jeton dans SonarQube (Mon compte › Sécurité) "
                       "puis « SonarQube : se connecter à un serveur… ».")
        # SonarQube 10 wants 12+ characters with upper, lower, digit and symbol.
        password = secrets.token_urlsafe(18) + "aA1!"
        api(cfg, "/api/users/change_password", {"login": "admin", "previousPassword": "admin", "password": password},
            method="POST", auth=("admin", "admin"))
        write_secret(LOCAL_SERVER, {"admin_password": password})
        print(f"Mot de passe admin par défaut remplacé (voir {LOCAL_SERVER}).")
    name = f"forgecode-{platform.node()}-{int(time.time())}"
    token = api(cfg, "/api/user_tokens/generate", {"name": name}, method="POST", auth=("admin", password))["token"]
    print(f"Jeton « {name} » créé.")
    return token


def try_auth(cfg, auth):
    try:
        return api(cfg, "/api/authentication/validate", auth=auth).get("valid", False)
    except Fail:
        return False


def local_stop():
    compose("stop")
    forge("notify", message="SonarQube local arrêté (données conservées)")


def configure():
    cfg = load_config()
    if not config_path().exists() or not cfg.get("url"):
        cfg = {
            "mode": "remote",
            "url": "https://sonarqube.example.com",
            "token": "COLLEZ_ICI_UN_JETON (Mon compte › Sécurité › Générer un jeton)",
            "organization": "",
        }
        save_config(cfg)
    print(f"Renseignez url et token dans {config_path()}, enregistrez, puis lancez « SonarQube : état ».")
    print("Pour SonarCloud : url = https://sonarcloud.io et organization = votre organisation.")
    forge("open", path=str(config_path()))


def status():
    cfg = load_config()
    if not cfg.get("url"):
        print("Aucun serveur configuré.")
        return
    print(f"Serveur : {cfg['url']} ({cfg.get('mode', 'remote')})")
    state = server_status(cfg)
    print(f"État : {state}")
    if state == "UP":
        print("Jeton : " + ("valide" if token_valid(cfg) else "INVALIDE ou absent"))
        print(f"Projet : {project_key(workspace())}")


# ---------------------------------------------------------------- analysis

def scanner_command(cfg, root, key):
    props = [
        f"-Dsonar.projectKey={key}",
        f"-Dsonar.projectName={root.name}",
        "-Dsonar.sources=.",
        f"-Dsonar.exclusions={EXCLUSIONS}",
        "-Dsonar.sourceEncoding=UTF-8",
    ]
    if cfg.get("organization"):
        props.append(f"-Dsonar.organization={cfg['organization']}")
    if any(root.rglob("*.java")):
        props.append("-Dsonar.java.binaries=.")
    # Explicit properties in sonar-project.properties win: drop ours.
    file_props = root / "sonar-project.properties"
    if file_props.exists():
        props = [p for p in props if p.startswith("-Dsonar.projectKey")]
    local = shutil.which("sonar-scanner")
    if local:
        env = dict(os.environ, SONAR_HOST_URL=cfg["url"], SONAR_TOKEN=cfg["token"])
        return [local, *props], env
    if not shutil.which("docker"):
        raise Fail("Ni sonar-scanner ni Docker trouvés : installez l'un des deux.")
    # The scanner runs in a container: localhost there is the container itself.
    url = re.sub(r"//(localhost|127\.0\.0\.1)", "//host.docker.internal", cfg["url"])
    cmd = ["docker", "run", "--rm", "-e", f"SONAR_HOST_URL={url}", "-e", f"SONAR_TOKEN={cfg['token']}",
           "-v", f"{root}:/usr/src"]
    if platform.system() == "Linux":
        cmd += ["--add-host", "host.docker.internal:host-gateway", "--user", f"{os.getuid()}:{os.getgid()}"]
    return cmd + ["sonarsource/sonar-scanner-cli", *props], None


def analyze():
    cfg = load_config()
    require_server(cfg)
    root = workspace()
    key = project_key(root)
    if server_status(cfg) != "UP":
        raise Fail(f"{cfg['url']} ne répond pas" + (" : lancez « SonarQube : démarrer un serveur local »." if cfg.get("mode") == "local" else "."))
    cmd, env = scanner_command(cfg, root, key)
    shown = [c if "SONAR_TOKEN" not in c else "SONAR_TOKEN=***" for c in cmd]
    print("$ " + " ".join(shown))
    started = time.time()
    if subprocess.run(cmd, cwd=root, env=env).returncode != 0:
        raise Fail("L'analyse a échoué (voir ci-dessus).")
    wait_for_report(cfg, root)
    print(f"\nAnalyse terminée en {time.time() - started:.0f} s.")
    summary(cfg, key)
    if not (root / ".gitignore").exists() or ".scannerwork" not in (root / ".gitignore").read_text(errors="replace"):
        print("Astuce : ajoutez .scannerwork/ à votre .gitignore.")


def wait_for_report(cfg, root):
    report = root / ".scannerwork" / "report-task.txt"
    if not report.exists():
        return
    task = dict(line.split("=", 1) for line in report.read_text().splitlines() if "=" in line).get("ceTaskId")
    if not task:
        return
    print("Traitement du rapport par le serveur…")
    for _ in range(240):
        state = api(cfg, "/api/ce/task", {"id": task})["task"]["status"]
        if state == "SUCCESS":
            return
        if state in ("FAILED", "CANCELED"):
            raise Fail(f"Le serveur n'a pas pu traiter l'analyse ({state}).")
        time.sleep(2)


def summary(cfg, key):
    gate = api(cfg, "/api/qualitygates/project_status", {"projectKey": key})["projectStatus"]["status"]
    metrics = "bugs,vulnerabilities,security_hotspots,code_smells,coverage,duplicated_lines_density,ncloc"
    measures = {m["metric"]: m.get("value", "-") for m in
                api(cfg, "/api/measures/component", {"component": key, "metricKeys": metrics})["component"]["measures"]}
    names = [("ncloc", "Lignes de code"), ("bugs", "Bugs"), ("vulnerabilities", "Vulnérabilités"),
             ("security_hotspots", "Points sensibles"), ("code_smells", "Code smells"),
             ("coverage", "Couverture %"), ("duplicated_lines_density", "Duplication %")]
    print(f"\nQuality gate : {'✓ OK' if gate == 'OK' else '✗ ' + gate}")
    for metric, label in names:
        if metric in measures:
            print(f"  {label:<18} {measures[metric]}")
    print()
    count = print_issues(cfg, key, None, None)
    forge("notify", message=f"SonarQube : quality gate {gate}, {count} problème(s)")


# ---------------------------------------------------------------- issues and rules

def fetch_issues(cfg, key, file=None, severity=None):
    params = {"componentKeys": key, "resolved": "false", "ps": 500, "s": "FILE_LINE"}
    if severity:
        severity = severity.upper()
        if severity not in SEVERITIES:
            raise Fail(f"Sévérité inconnue : {severity} ({', '.join(SEVERITIES)})")
        params["severities"] = ",".join(SEVERITIES[SEVERITIES.index(severity):])
    issues, page = [], 1
    while True:
        data = api(cfg, "/api/issues/search", {**params, "p": page})
        issues += data.get("issues", [])
        if page * 500 >= min(data.get("paging", {}).get("total", 0), 10000):
            break
        page += 1
    for issue in issues:
        issue["path"] = issue["component"].split(":", 1)[-1] if ":" in issue["component"] else ""
    if file:
        issues = [i for i in issues if i["path"] == file]
    return issues


def print_issues(cfg, key, file, severity):
    issues = fetch_issues(cfg, key, file, severity)
    if not issues:
        print("Aucun problème ouvert" + (f" dans {file}" if file else "") + ".")
        return 0
    order = {s: i for i, s in enumerate(reversed(SEVERITIES))}
    for issue in sorted(issues, key=lambda i: (i["path"], i.get("line", 0), order.get(i.get("severity"), 9))):
        kind = issue.get("type", "").replace("_", " ").lower()
        print(f"{issue['path']}:{issue.get('line', 1)}: {issue.get('severity', '?')} [{kind}] {issue['message']} ({issue['rule']})")
    print(f"\n{len(issues)} problème(s).")
    return len(issues)


def relative_current_file(root):
    current = os.environ.get("FORGE_FILE")
    if not current:
        raise Fail("Aucun fichier ouvert.")
    try:
        return Path(current).resolve().relative_to(root).as_posix()
    except ValueError:
        raise Fail("Le fichier courant n'est pas dans le projet.")


def issues(args):
    cfg = load_config()
    require_server(cfg)
    root = workspace()
    file = os.environ.get("FORGE_ARG_file") or None
    if "--file" in args:
        file = relative_current_file(root)
    print_issues(cfg, project_key(root), file, os.environ.get("FORGE_ARG_severity") or None)


def fix():
    cfg = load_config()
    require_server(cfg)
    root = workspace()
    file = relative_current_file(root)
    found = fetch_issues(cfg, project_key(root), file)
    if not found:
        print(f"Aucun problème SonarQube dans {file}.")
        return
    lines = "\n".join(f"- ligne {i.get('line', '?')} : {i['message']} ({i['rule']}, {i.get('severity')})" for i in found)
    forge("chat", message=f"Corrige ces problèmes SonarQube dans `{file}` sans changer le comportement. "
                          f"Utilise l'outil sonarqube_rule si une règle n'est pas claire.\n{lines}")
    print(f"{len(found)} problème(s) envoyés au chat.")


def rule():
    cfg = load_config()
    require_server(cfg)
    key = os.environ.get("FORGE_ARG_key") or (sys.argv[2] if len(sys.argv) > 2 else "")
    if not key:
        raise Fail("Indiquez la clé de la règle (ex. python:S1192).")
    r = api(cfg, "/api/rules/show", {"key": key})["rule"]
    print(f"{r['key']} - {r['name']} ({r.get('severity', '')}, {r.get('type', '')})\n")
    sections = r.get("descriptionSections") or [{"content": r.get("htmlDesc", "")}]
    for section in sections:
        text = re.sub(r"<pre>(.*?)</pre>", lambda m: "\n" + m.group(1) + "\n", section.get("content", ""), flags=re.S)
        text = re.sub(r"<[^>]+>", "", text)
        for entity, char in (("&lt;", "<"), ("&gt;", ">"), ("&quot;", '"'), ("&#39;", "'"), ("&amp;", "&")):
            text = text.replace(entity, char)
        print(re.sub(r"\n{3,}", "\n\n", text).strip() + "\n")


def dashboard():
    cfg = load_config()
    require_server(cfg)
    url = f"{cfg['url']}/dashboard?id={urllib.parse.quote(project_key(workspace()))}"
    print(url)
    webbrowser.open(url)


COMMANDS = {
    "analyze": lambda args: analyze(),
    "issues": issues,
    "fix": lambda args: fix(),
    "rule": lambda args: rule(),
    "local-start": lambda args: local_start(),
    "local-stop": lambda args: local_stop(),
    "configure": lambda args: configure(),
    "dashboard": lambda args: dashboard(),
    "status": lambda args: status(),
}


def main():
    if len(sys.argv) < 2 or sys.argv[1] not in COMMANDS:
        print(__doc__)
        return 2
    try:
        COMMANDS[sys.argv[1]](sys.argv[2:])
    except Fail as e:
        print(f"Erreur : {e}", file=sys.stderr)
        forge("notify", message=f"SonarQube : {e}")
        return 1
    except KeyboardInterrupt:
        return 130
    return 0


if __name__ == "__main__":
    sys.exit(main())
