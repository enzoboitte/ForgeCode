#!/usr/bin/env python3
"""ForgeCode module: Conan 2, the C/C++ package manager.

Installs Conan into the module's own folder when it is missing, creates a conanfile.txt ready for
CMake, adds packages from ConanCenter (latest version), installs them and configures CMake with
Conan's toolchain and a compile_commands.json for clangd.

Usage: conan_module.py new | add <name> | search <text> | install [Release|Debug] | configure [Release|Debug]
       | build [Release|Debug] | outdated | launch-json

The compiler is the one ForgeCode was set up with (CC / CXX, or MSVC's environment on Windows).
"""

import json
import os
import re
import shutil
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
WORKSPACE = os.environ.get("FORGE_WORKSPACE") or os.getcwd()
WINDOWS = os.name == "nt"

for stream in (sys.stdout, sys.stderr):
    try:
        stream.reconfigure(encoding="utf-8", line_buffering=True)
    except AttributeError:
        pass

TEMPLATE = """[requires]

[generators]
CMakeDeps
CMakeToolchain

[layout]
cmake_layout
"""


class Fail(Exception):
    pass


def forge(action, **fields):
    """An action for ForgeCode (open a file, notify…), on its own line."""
    print("@forge " + json.dumps({"action": action, **fields}), flush=True)


def conan():
    """Conan on the PATH, else the one installed in the module (installed now if needed)."""
    found = shutil.which("conan")
    if found:
        return found
    runtime = os.path.join(HERE, "runtime")
    exe = os.path.join(runtime, "Scripts" if WINDOWS else "bin", "conan.exe" if WINDOWS else "conan")
    if not os.path.exists(exe):
        # On stderr: the panel reads stdout as JSON.
        print("Conan n'est pas installé : installation dans le module (une seule fois)…", file=sys.stderr, flush=True)
        import venv

        venv.create(runtime, with_pip=True)
        python = os.path.join(runtime, "Scripts" if WINDOWS else "bin", "python.exe" if WINDOWS else "python")
        subprocess.check_call([python, "-m", "pip", "install", "--quiet", "--disable-pip-version-check", "conan>=2,<3"])
    return exe


def run(args, capture=False, env=None):
    """`conan <args>` in the project; its output, when captured."""
    exe = conan()
    if not capture:
        print("$ conan " + " ".join(args), flush=True)
        code = subprocess.call([exe] + args, cwd=WORKSPACE, env=env)
        if code != 0:
            raise Fail(f"conan {args[0]} a échoué (code {code})")
        return ""
    out = subprocess.run([exe] + args, cwd=WORKSPACE, capture_output=True, text=True, env=env)
    if out.returncode != 0:
        raise Fail((out.stderr or out.stdout).strip() or f"conan {args[0]} a échoué")
    return out.stdout


def conanfile():
    for name in ("conanfile.py", "conanfile.txt"):
        path = os.path.join(WORKSPACE, name)
        if os.path.exists(path):
            return path
    return None


def version_key(version):
    """`1.10.2` > `1.9`: numbers compared as numbers, other parts as text after them."""
    parts = re.split(r"[.\-+]", version)
    return [(0, int(p), "") if p.isdigit() else (1, 0, p) for p in parts]


def latest(name):
    """The newest `name/version` on ConanCenter, or None."""
    out = run(["search", f"{name}/*", "-r", "conancenter", "--format=json"], capture=True)
    refs = [ref for remote in json.loads(out or "{}").values() if isinstance(remote, dict) for ref in remote if ref.split("/")[0] == name]
    return max(refs, key=lambda r: version_key(r.split("/", 1)[1]), default=None)


def add_requirement(text, ref):
    """conanfile.txt with `ref` in [requires]: replaces another version of the same package."""
    name = ref.split("/")[0]
    lines = text.splitlines()
    start = next((i for i, l in enumerate(lines) if l.strip().lower() == "[requires]"), None)
    if start is None:
        return f"[requires]\n{ref}\n\n" + text
    end = next((i for i in range(start + 1, len(lines)) if lines[i].strip().startswith("[")), len(lines))
    for i in range(start + 1, end):
        if lines[i].strip().split("/")[0] == name:
            lines[i] = ref
            return "\n".join(lines) + "\n"
    # After the last requirement, before the blank lines that end the section.
    at = end
    while at > start + 1 and not lines[at - 1].strip():
        at -= 1
    lines.insert(at, ref)
    return "\n".join(lines) + "\n"


def cmd_new():
    path = conanfile()
    if path:
        print(f"{os.path.basename(path)} existe déjà.")
    else:
        path = os.path.join(WORKSPACE, "conanfile.txt")
        with open(path, "w", encoding="utf-8") as f:
            f.write(TEMPLATE)
        print("conanfile.txt créé (générateurs CMakeDeps et CMakeToolchain, layout CMake).")
    forge("open", path=os.path.relpath(path, WORKSPACE))


def cmd_add(name):
    name = (name or os.environ.get("FORGE_SELECTION", "")).strip()
    if not name:
        raise Fail("Sélectionnez le nom du paquet (ex. fmt) dans l'éditeur, puis relancez la commande.")
    ref = name if "/" in name else latest(name.lower())
    if not ref:
        raise Fail(f"Aucun paquet « {name} » sur ConanCenter. Essayez « Conan : chercher un paquet ».")
    path = conanfile()
    if path and path.endswith(".py"):
        print(f"Ajoutez dans requirements() de conanfile.py : self.requires(\"{ref}\")")
        forge("open", path="conanfile.py")
        return
    if not path:
        path = os.path.join(WORKSPACE, "conanfile.txt")
        with open(path, "w", encoding="utf-8") as f:
            f.write(TEMPLATE)
    with open(path, encoding="utf-8") as f:
        text = f.read()
    with open(path, "w", encoding="utf-8") as f:
        f.write(add_requirement(text, ref))
    print(f"{ref} ajouté à conanfile.txt. Lancez « Conan : installer les dépendances ».")
    forge("notify", message=f"{ref} ajouté")
    forge("open", path="conanfile.txt")


def cmd_search(text):
    text = (text or os.environ.get("FORGE_SELECTION", "")).strip()
    if not text:
        raise Fail("Sélectionnez un mot à chercher (ex. json) dans l'éditeur.")
    out = run(["search", f"*{text}*", "-r", "conancenter", "--format=json"], capture=True)
    refs = [ref for remote in json.loads(out or "{}").values() if isinstance(remote, dict) for ref in remote]
    latest_by_name = {}
    for ref in refs:
        name, version = ref.split("/", 1)
        if name not in latest_by_name or version_key(version) > version_key(latest_by_name[name]):
            latest_by_name[name] = version
    if not latest_by_name:
        print(f"Aucun paquet ne correspond à « {text} ».")
    for name in sorted(latest_by_name):
        print(f"{name}/{latest_by_name[name]}")


def msvc_dir():
    """Visual Studio whose environment is active (set up by ForgeCode, or a Developer Prompt)."""
    return os.environ.get("VSINSTALLDIR") if WINDOWS else None


def use_ninja(build_dir):
    """Ninja (shipped with Visual Studio) under MSVC's environment: it works wherever Visual Studio is
    installed, unlike the Visual Studio generator. Not for a folder configured with another one."""
    if os.path.exists(os.path.join(WORKSPACE, build_dir, "CMakeCache.txt")):
        return False
    return bool(msvc_dir()) and shutil.which("ninja") is not None


def detect_env():
    """`conan profile detect` sees the compiler ForgeCode uses: CC / CXX, else MSVC's `cl`."""
    env = dict(os.environ)
    if msvc_dir() and not env.get("CC"):
        env["CC"], env["CXX"] = "cl", "cl"
    return env


def compiler_conf(build_dir):
    """Conan settings making CMake use that same compiler."""
    conf = []
    cc, cxx = os.environ.get("CC"), os.environ.get("CXX")
    if cc or cxx:
        compilers = {k: v for k, v in (("c", cc), ("cpp", cxx)) if v}
        conf += ["-c", "tools.build:compiler_executables=" + json.dumps(compilers)]
    if msvc_dir():
        conf += ["-c", f"tools.microsoft.msbuild:installation_path={msvc_dir().rstrip(chr(92))}"]
    if use_ninja(build_dir):
        conf += ["-c", "tools.cmake.cmaketoolchain:generator=Ninja"]
    return conf


def build_type(arg):
    return "Debug" if (arg or "").lower() == "debug" else "Release"


def profiles():
    """The user's Conan profiles (`~/.conan2/profiles`), `default` first."""
    home = os.environ.get("CONAN_HOME") or os.path.join(os.path.expanduser("~"), ".conan2")
    try:
        names = sorted(n for n in os.listdir(os.path.join(home, "profiles")) if not n.startswith("."))
    except OSError:
        names = []
    return sorted(set(names) | {"default"}, key=lambda n: (n != "default", n))


def settings_args(bt, profile):
    """Build type, profile (host and build, like `-pr:h=mingw -pr:b=mingw`) and compiler for Conan."""
    args = ["--build=missing", "-s", f"build_type={bt}"]
    if profile and profile != "default":
        args += [f"-pr:h={profile}", f"-pr:b={profile}"]
    else:
        run(["profile", "detect", "--exist-ok"], capture=True, env=detect_env())
    return args + compiler_conf(os.path.join("build", bt))


def cmd_install(arg, profile=""):
    path = conanfile()
    if not path:
        raise Fail("Pas de conanfile.txt ni de conanfile.py : lancez « Conan : créer un conanfile.txt ».")
    bt = build_type(arg)
    args = ["install", "."] + settings_args(bt, profile)
    with open(path, encoding="utf-8") as f:
        text = f.read()
    # Without a layout Conan writes its files at the project's root: keep them in build/.
    if "layout" not in text:
        args += ["--output-folder", os.path.join("build", "conan")]
    run(args)
    forge("notify", message=f"Dépendances Conan installées ({bt})")
    return bt


def toolchain():
    """The conan_toolchain.cmake Conan just wrote (the newest)."""
    found = []
    for root, dirs, files in os.walk(os.path.join(WORKSPACE, "build")):
        if "conan_toolchain.cmake" in files:
            found.append(os.path.join(root, "conan_toolchain.cmake"))
    return max(found, key=os.path.getmtime, default=None)


def preset(bt):
    """The configure and build presets Conan's CMakeToolchain wrote for `bt` (newest first): they
    carry the generator and compiler of the profile (`MinGW Makefiles`, Ninja…)."""
    found = []
    for root, _, files in os.walk(os.path.join(WORKSPACE, "build")):
        if "CMakePresets.json" in files:
            found.append(os.path.join(root, "CMakePresets.json"))
    for path in sorted(found, key=os.path.getmtime, reverse=True):
        try:
            with open(path, encoding="utf-8") as f:
                data = json.load(f)
        except (OSError, ValueError):
            continue
        configure = [p for p in data.get("configurePresets", []) if not p.get("hidden")]
        mine = [p for p in configure if p.get("cacheVariables", {}).get("CMAKE_BUILD_TYPE") == bt or p["name"].lower().endswith(bt.lower())]
        # Multi-config generators (Visual Studio) have one `conan-default` for every build type.
        chosen = (mine or configure or [None])[0]
        if not chosen:
            continue
        builds = [b["name"] for b in data.get("buildPresets", []) if b.get("configurePreset") == chosen["name"]]
        build = next((b for b in builds if b.lower().endswith(bt.lower())), builds[0] if builds else None)
        return chosen["name"], build
    return None


def copy_compile_commands():
    """clangd reads compile_commands.json at the root: the dependencies' headers are found."""
    found = []
    for root, _, files in os.walk(os.path.join(WORKSPACE, "build")):
        if "compile_commands.json" in files:
            found.append(os.path.join(root, "compile_commands.json"))
    if found:
        shutil.copyfile(max(found, key=os.path.getmtime), os.path.join(WORKSPACE, "compile_commands.json"))
        print("compile_commands.json copié à la racine (pour clangd).")


def call(args, error):
    print("$ " + " ".join(args), flush=True)
    if subprocess.call(args, cwd=WORKSPACE) != 0:
        raise Fail(error)


def cmd_configure(arg, profile=""):
    bt = cmd_install(arg, profile)
    cmake = shutil.which("cmake") or "cmake"
    found = preset(bt)
    if found:
        call([cmake, "--preset", found[0], "-DCMAKE_EXPORT_COMPILE_COMMANDS=ON"], "cmake a échoué")
        copy_compile_commands()
        forge("notify", message=f"CMake configuré avec Conan ({bt}, preset {found[0]})")
        return found
    tc = toolchain()
    if not tc:
        raise Fail("conan_toolchain.cmake introuvable : vérifiez que le générateur CMakeToolchain est dans le conanfile.")
    out = os.path.join("build", bt)
    cmake = shutil.which("cmake") or "cmake"
    args = [cmake, "-S", ".", "-B", out, f"-DCMAKE_TOOLCHAIN_FILE={tc}", f"-DCMAKE_BUILD_TYPE={bt}", "-DCMAKE_EXPORT_COMPILE_COMMANDS=ON"]
    if use_ninja(out):
        args += ["-G", "Ninja"]
    print("$ " + " ".join(args), flush=True)
    if subprocess.call(args, cwd=WORKSPACE) != 0:
        raise Fail("cmake a échoué")
    # clangd reads compile_commands.json at the root: the dependencies' headers are found.
    db = os.path.join(WORKSPACE, out, "compile_commands.json")
    if os.path.exists(db):
        shutil.copyfile(db, os.path.join(WORKSPACE, "compile_commands.json"))
        print("compile_commands.json copié à la racine (pour clangd).")
    print(f"Compiler ensuite : cmake --build {out}")
    forge("notify", message=f"CMake configuré avec Conan ({bt})")
    return None


def run_env(bt):
    """What Conan's `conanrun` script sets (the folders of the dependencies' shared libraries)."""
    name = "conanrun.bat" if WINDOWS else "conanrun.sh"
    found = []
    for root, _, files in os.walk(os.path.join(WORKSPACE, "build")):
        if name in files:
            found.append(os.path.join(root, name))
    # build/Debug/…, build/windows-gcc-16-debug/…
    mine = [f for f in found if bt.lower() in os.path.relpath(f, WORKSPACE).lower()] or found
    if not mine:
        return {}
    script = max(mine, key=os.path.getmtime)
    if WINDOWS:
        out = subprocess.run(f'call "{script}" >nul && set', shell=True, capture_output=True, text=True)
    else:
        out = subprocess.run(["sh", "-c", f'. "{script}" >/dev/null && env'], capture_output=True, text=True)
    env = {}
    for line in out.stdout.splitlines():
        key, sep, value = line.partition("=")
        # The shell's own bookkeeping is not the program's environment.
        if sep and key and key not in ("SHLVL", "_", "PWD", "OLDPWD") and os.environ.get(key) != value:
            env[key] = value
    return env


def cmd_build(arg, profile=""):
    """Build for the debugger (F5), then tell ForgeCode the program's environment. A conanfile.py
    builds with its own build() (`conan build`, as by hand); a conanfile.txt through CMake."""
    bt = build_type(arg)
    path = conanfile()
    if path and path.endswith(".py"):
        run(["build", "."] + settings_args(bt, profile))
        copy_compile_commands()
    else:
        found = cmd_configure(bt, profile)
        cmake = shutil.which("cmake") or "cmake"
        if found and found[1]:
            call([cmake, "--build", "--preset", found[1], "--parallel"], "la compilation a échoué")
        else:
            call([cmake, "--build", os.path.join("build", bt), "--config", bt, "--parallel"], "la compilation a échoué")
    forge("launch", env=run_env(bt), cwd=WORKSPACE)


def json_launch():
    """The ways to debug the project with Conan, for ForgeCode's F5 picker."""
    if not conanfile():
        return emit([])
    how = "conan build" if conanfile().endswith(".py") else "conan install + CMake + build"
    emit([
        {
            "label": f"Conan | {bt}" + ("" if profile == "default" else f" | {profile}"),
            "detail": f"{how} (profil {profile})",
            "build": f"conan-forge build {bt.lower()} {profile}",
            "program_dir": "build",
        }
        for profile in profiles()
        for bt in ("Debug", "Release")
    ])


def cmd_outdated():
    if not conanfile():
        raise Fail("Pas de conanfile dans le projet.")
    run(["graph", "outdated", "."])


def requirements(text):
    """The references in conanfile.txt's [requires]."""
    refs, inside = [], False
    for line in text.splitlines():
        s = line.split("#")[0].strip()
        if s.startswith("["):
            inside = s.lower() == "[requires]"
        elif inside and s:
            refs.append(s)
    return refs


def remove_requirement(text, name):
    """conanfile.txt without package `name` in [requires]."""
    out, inside = [], False
    for line in text.splitlines():
        s = line.strip()
        if s.startswith("["):
            inside = s.lower() == "[requires]"
        if inside and s and not s.startswith("[") and s.split("/")[0] == name:
            continue
        out.append(line)
    return "\n".join(out) + "\n"


def emit(value):
    """A JSON answer for ForgeCode's Conan panel, on stdout."""
    print(json.dumps(value), flush=True)


def json_requires():
    path = conanfile()
    if not path:
        return emit({"file": None, "requires": []})
    if path.endswith(".py"):
        # conanfile.py: its requires as Conan reads them.
        out = run(["inspect", ".", "--format=json"], capture=True)
        reqs = json.loads(out).get("requires") or []
        refs = [r.get("ref", r) if isinstance(r, dict) else r for r in (reqs.values() if isinstance(reqs, dict) else reqs)]
        return emit({"file": "conanfile.py", "requires": [str(r) for r in refs]})
    with open(path, encoding="utf-8") as f:
        emit({"file": "conanfile.txt", "requires": requirements(f.read())})


def json_remotes():
    remotes = json.loads(run(["remote", "list", "--format=json"], capture=True) or "[]")
    emit([{"name": r.get("name"), "url": r.get("url"), "enabled": r.get("enabled", True)} for r in remotes])


CATALOG = os.path.join(HERE, "catalog.json")


def json_catalog(refresh):
    """Every package of the enabled remotes, newest version each; kept a day (ConanCenter is big)."""
    import time

    if not refresh and os.path.exists(CATALOG) and time.time() - os.path.getmtime(CATALOG) < 86400:
        with open(CATALOG, encoding="utf-8") as f:
            return emit(json.load(f))
    out = run(["search", "*", "-r", "*", "--format=json"], capture=True)
    newest = {}
    for remote, refs in json.loads(out or "{}").items():
        if not isinstance(refs, dict):
            continue
        for ref in refs:
            if "/" not in ref:
                continue
            name, version = ref.split("/", 1)
            if name not in newest or version_key(version) > version_key(newest[name][0]):
                newest[name] = (version, remote)
    packages = [{"name": n, "version": v, "remote": r} for n, (v, r) in sorted(newest.items())]
    with open(CATALOG, "w", encoding="utf-8") as f:
        json.dump(packages, f)
    emit(packages)


def cmd_remote_add(url, name):
    """A Conan server by its URL, or a Git repository of recipes (cloned, then indexed)."""
    url = url.strip()
    if not url:
        raise Fail("Donnez le lien du dépôt.")
    name = (name or "").strip() or re.sub(r"[^A-Za-z0-9_-]", "-", url.rstrip("/").split("/")[-1].removesuffix(".git")) or "remote"
    if url.endswith(".git") or "github.com/" in url or "gitlab" in url:
        folder = os.path.join(HERE, "indexes", name)
        if os.path.exists(folder):
            subprocess.check_call(["git", "-C", folder, "pull", "--ff-only"])
        else:
            subprocess.check_call(["git", "clone", "--depth", "1", url, folder])
        run(["remote", "add", name, folder, "--type=local-recipes-index", "--force"], capture=True)
    else:
        run(["remote", "add", name, url, "--force"], capture=True)
    if os.path.exists(CATALOG):
        os.remove(CATALOG)
    print(f"Dépôt « {name} » ajouté : {url}")


def cmd_remote_remove(name):
    run(["remote", "remove", name], capture=True)
    if os.path.exists(CATALOG):
        os.remove(CATALOG)
    print(f"Dépôt « {name} » retiré.")


def cmd_remove(name):
    path = conanfile()
    if not path or path.endswith(".py"):
        raise Fail("Retirer une dépendance se fait dans conanfile.py lui-même.")
    with open(path, encoding="utf-8") as f:
        text = f.read()
    with open(path, "w", encoding="utf-8") as f:
        f.write(remove_requirement(text, name.split("/")[0]))
    print(f"{name} retiré de conanfile.txt.")


def main(argv):
    command, arg = (argv + ["", ""])[:2]
    extra = argv[2] if len(argv) > 2 else ""
    arg = arg or os.environ.get("FORGE_ARG_name") or os.environ.get("FORGE_ARG_query") or os.environ.get("FORGE_ARG_build_type") or ""
    actions = {
        "new": lambda: cmd_new(),
        "add": lambda: cmd_add(arg),
        "search": lambda: cmd_search(arg),
        "install": lambda: cmd_install(arg, extra),
        "configure": lambda: cmd_configure(arg, extra),
        "build": lambda: cmd_build(arg, extra),
        "launch-json": lambda: json_launch(),
        "outdated": lambda: cmd_outdated(),
        # For ForgeCode's Conan panel.
        "requires-json": lambda: json_requires(),
        "remotes-json": lambda: json_remotes(),
        "catalog-json": lambda: json_catalog(arg == "refresh"),
        "remote-add": lambda: cmd_remote_add(arg, extra),
        "remote-remove": lambda: cmd_remote_remove(arg),
        "remove": lambda: cmd_remove(arg),
    }
    if command not in actions:
        print(__doc__)
        return 2
    try:
        actions[command]()
        return 0
    except Fail as e:
        print(f"Erreur : {e}", file=sys.stderr)
        forge("notify", message=str(e))
        return 1
    except subprocess.CalledProcessError as e:
        print(f"Erreur : {e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
