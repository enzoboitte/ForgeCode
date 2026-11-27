# Conan pour ForgeCode

Gère les dépendances C/C++ d'un projet CMake avec [Conan 2](https://conan.io) : ajoute des paquets de ConanCenter, les installe et configure CMake avec la toolchain de Conan.

## Prérequis

- Python 3.9 ou plus récent (`python3` sous macOS et Linux, `py` ou `python` sous Windows).
- CMake et un compilateur C++.
- Conan 2 : s'il n'est pas dans le `PATH`, le module l'installe lui-même la première fois, dans son dossier `runtime/` (environnement Python séparé, rien n'est installé ailleurs).

## Démarrage

1. « Conan : créer un conanfile.txt » : `[requires]` vide, générateurs `CMakeDeps` et `CMakeToolchain`, layout CMake.
2. Sélectionnez le nom d'un paquet dans l'éditeur (`fmt`, `sfml`, `nlohmann_json`…) puis « Conan : ajouter le paquet sélectionné » : sa dernière version sur ConanCenter est ajoutée à `[requires]` (la version d'un paquet déjà présent est remplacée). Une référence complète (`fmt/10.2.1`) est ajoutée telle quelle.
3. « Conan : configurer CMake avec Conan (Release) » : installe les dépendances (`conan install . --build=missing`, profil détecté automatiquement), puis lance CMake dans `build/Release` avec `conan_toolchain.cmake`. `compile_commands.json` est copié à la racine : clangd trouve les en-têtes des dépendances (plus d'erreur « file not found »).
4. Compilez : `cmake --build build/Release`.

Dans le `CMakeLists.txt`, utilisez les cibles que Conan fournit, par exemple :

```cmake
find_package(fmt REQUIRED)
target_link_libraries(app PRIVATE fmt::fmt)
```

## Commandes

| Commande | Effet |
|---|---|
| Installer les dépendances | `conan install` en Release (bouton dans la barre d'état) |
| Configurer CMake avec Conan (Release / Debug) | installe puis configure CMake, `compile_commands.json` pour clangd |
| Ajouter le paquet sélectionné | dernière version ConanCenter dans `[requires]` (bouton dans la barre de l'éditeur) |
| Chercher un paquet | paquets ConanCenter dont le nom contient le texte sélectionné, avec leur dernière version |
| Créer un conanfile.txt | modèle prêt pour CMake |
| Dépendances obsolètes | `conan graph outdated` |

Avec un `conanfile.py`, « ajouter » indique la ligne `self.requires(...)` à écrire. Sans `[layout]` dans le `conanfile.txt`, les fichiers générés vont dans `build/conan` au lieu de la racine.

Outils pour l'IA : `conan_search`, `conan_add`, `conan_install` : le chat peut chercher, ajouter et installer une bibliothèque lui-même.
