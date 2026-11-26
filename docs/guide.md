# Guide d'utilisation

## Démarrer

- Ouvrir un projet : `forgecode <dossier>`, ⌘O, ou un projet récent sur l'écran d'accueil.
- Créer un projet : menu **Fichier › Nouveau projet…** (C, C++, Rust, TypeScript, Web).
- Les menus (ForgeCode, Fichier, Édition, Affichage, Exécuter, Terminal, IA, Modules) sont dans la barre de menus du système sur macOS et en haut de la fenêtre sous Windows et Linux.
- Tout est accessible depuis la **palette de commandes** (⌘⇧P).

## Interface

| Zone | Contenu |
|---|---|
| Barre de gauche | Explorateur, Recherche, Contrôle de code source, compte GitHub, Paramètres |
| Centre | Onglets : fichiers, images, diffs, historique Git, agents. Glisser un onglet ou un fichier sur un bord pour diviser l'écran |
| Bas (⌘J) | Problèmes, Sortie, Terminaux, Débogage |
| Droite (⌥⌘B) | Chat IA |

Les fichiers sont **enregistrés automatiquement** (réglable dans Paramètres › Éditeur). Tab et le formatage suivent l'indentation déjà utilisée dans le fichier (tabulations ou 2/4 espaces) ; sinon la taille de tabulation des paramètres.

## Raccourcis

Sous Linux et Windows, ⌘ devient Ctrl et ⌥ devient Alt (l'interface affiche les raccourcis de votre système).

| Raccourci | Action |
|---|---|
| ⌘⇧P / ⌘P | palette de commandes / aller à un fichier |
| ⌘⇧F | rechercher et remplacer dans le projet |
| F12 ou ⌘-clic | aller à la définition |
| ⇧⌥F | formater le document |
| ⌃Espace | complétion (comme VS Code : filtrage approximatif, icône du type, signature et détail, Entrée pour accepter) |
| ⌘ + molette | zoom : taille du texte dans l'éditeur, zoom centré sur le curseur dans les PDF et les images |
| Tab / ⌘→ | accepter la suggestion Copilot / son mot suivant |
| ⌘R / F5 | exécuter / déboguer |
| F9 ou clic sur un numéro de ligne | point d'arrêt |
| F10 / F11 / ⇧F11 | pas à pas : principal / détaillé / sortant |
| ⌃\` / ⌃⇧\` | terminal / nouveau terminal |
| ⌘\\ | diviser à droite |
| ⌘L | chat (avec du code sélectionné : le lier au chat) |
| ⌘, | paramètres |

## Édition

Comme dans VS Code : taper `"`, `'`, `` ` ``, `(`, `[` ou `{` sur une sélection l'entoure ; ⌘/ commente ou décommente ; ⌥↑/⌥↓ déplace la ligne ; ⇧⌥↑/⇧⌥↓ la duplique ; ⇧⌘K la supprime ; ⌘Entrée / ⇧⌘Entrée insère une ligne en dessous / au-dessus ; ⌘D ajoute l'occurrence suivante à la sélection, ⇧⌘L sélectionne toutes les occurrences (menu **Sélection**). La complétion s'affiche dès la frappe en filtrant localement la dernière réponse du serveur, qui est rafraîchie en arrière-plan.

## Langages

La coloration est intégrée (C, C++, Rust, JS/TS, HTML, CSS/SCSS/Less, PHP, Python, SQL, JSON, YAML, Go, Java, Ruby, Lua, Kotlin, C#, Bash, Markdown…). Complétion, survol, diagnostics, définition et formatage passent par un serveur de langage, **installé automatiquement** à la première ouverture d'un fichier du langage :

| Langage | Serveur |
|---|---|
| Rust | `rust-analyzer` (rustup ou téléchargé) |
| C, C++ | `clangd` (Xcode sur macOS, sinon téléchargé) |
| JS, TS, HTML, CSS, SCSS, Less, JSON | serveurs TypeScript et VS Code (npm) |
| PHP | `intelephense` (npm) |
| Python | `pyright` (npm) |
| YAML, Svelte, Bash | serveurs npm correspondants |

npm vient d'un Node.js portable s'il n'est pas installé. Tout va dans `<config>/servers` (`~/Library/Application Support/forgecode` sur macOS, `~/.config/forgecode` sur Linux, `%APPDATA%\forgecode` sur Windows). Il faut une connexion la première fois ; l'avancement et les erreurs s'affichent en bas à droite. Les fichiers JSON connus (`package.json`, `tsconfig.json`, `composer.json`, configurations ESLint/Prettier/Babel) sont validés contre leur schéma.

**Bibliothèques.** Python : l'environnement virtuel du projet (`.venv`, `venv` ou `env`) est utilisé par pyright (imports résolus), Exécuter, le débogueur et les terminaux (activé comme `source .venv/bin/activate`). Rust : les dépendances de `Cargo.toml` (rust-analyzer). JS/TS : `node_modules`. PHP : `vendor/` (Composer). Go : les modules. C/C++ : `compile_commands.json` (généré pour CMake) ; sans lui, clangd et la compilation d'un fichier seul cherchent les en-têtes dans `include/`, `src/`, `third_party/*/include`, `vcpkg_installed/*/include` et Homebrew. Pour un projet Makefile, générez `compile_commands.json` avec `bear -- make` pour des chemins exacts.

**Paramètres › Langages** règle chaque langage : niveau de **vérification des types** (désactivée, basique, standard, stricte) pour Python (pyright), PHP (intelephense) et JS/TS (fichiers hors `tsconfig`), interpréteur (Python, PHP, Node ; vide = environnement virtuel du projet puis PATH), arguments du programme, arrêt au début en débogage, « déboguer uniquement mon code » (Python).

**Exécuter (⌘R)** suit le projet : `cargo` (Rust), `CMakeLists.txt` ou `Makefile` (C/C++), `npm start` (JS/TS), sinon le fichier seul, avec l'interpréteur et les arguments réglés.

**Déboguer (F5)** installe le débogueur du langage la première fois :

| Langage | Débogueur | Remarque |
|---|---|---|
| C, C++, Rust, Zig, Swift | `lldb-dap` | Xcode sur macOS, paquet `lldb` ailleurs |
| Python | debugpy | installé dans un environnement dédié |
| JavaScript, TypeScript | js-debug (celui de VS Code) | TS via `tsx` du projet, sinon Node 22.6+ |
| PHP | vscode-php-debug | nécessite l'extension Xdebug dans PHP |
| Go | delve | installé avec `go install` |

Les **images** et les **PDF** s'ouvrent dans un onglet (zoom avec les boutons de la barre ou ⌘ + molette) ; les autres fichiers binaires dans l'app par défaut du système.

## Git

Vue Contrôle de code source (⌃⇧G) : indexer, annuler, valider (✨ génère le message), branches, pull/push. Cliquer un fichier ouvre le diff.

**Fusion et conflits.** Le menu de la branche (en haut du panneau) propose **Fusionner dans la branche actuelle** et **Rebaser la branche actuelle sur**. Synchroniser fusionne aussi automatiquement quand le dépôt distant et le local ont chacun des commits. En cas de conflit :

- un encadré indique l'opération en cours avec **Continuer** (actif quand tout est résolu) et **Annuler** ;
- la section **Conflits de fusion** liste les fichiers : garder la version actuelle ou entrante pour tout le fichier, ou marquer comme résolu après modification ;
- dans l'éditeur, les blocs en conflit sont colorés (actuel en vert, entrant en bleu) et une barre propose **Accepter l'actuel**, **Accepter l'entrant**, **Accepter les deux** pour le conflit sous le curseur, avec ▲ ▼ pour passer d'un conflit à l'autre.

Une erreur git reste affichée dans le panneau jusqu'à la prochaine action (ou ✕). Le **graphe de l'historique** (icône graphe, palette ou Affichage › Graphe de l'historique) montre toutes les branches dans l'ordre chronologique : une couleur par branche, les fusions dessinées, les étiquettes de branches et de tags ; cliquer un commit ouvre son diff.

## IA

### GitHub Copilot
Paramètres › IA › **Se connecter avec GitHub**. Plusieurs comptes possibles : choisir un compte ou **Auto** (répartition selon le quota restant, bascule si un compte est limité). Les suggestions apparaissent en gris pendant la frappe.

### Autres modèles
Paramètres › IA › **Serveurs de modèles** : llama.cpp, Ollama, LM Studio, serveur compatible OpenAI ou API Claude. Le modèle du chat se choisit sous la zone de saisie.

### Chat et agents
- `@` lie un fichier, `/` propose les commandes (`/fix`, `/tests`, `/explain`…), le trombone joint des fichiers ou la sélection.
- **Modes** : *Demander* (validation de chaque action), *Auto*, *Auto + sécurité* (un agent relecteur valide), *Plan* (lecture seule puis « Exécuter le plan »), *Intelligent* (l'agent délègue à des sous-agents et choisit leurs modèles).
- L'onglet **Agents** montre l'arbre des agents et leurs échanges en direct.

## Dossier `.forgecode/`

Créé dans chaque projet :

| Fichier | Rôle |
|---|---|
| `instructions.md` | consignes données à tous les agents |
| `agents/<rôle>.md` | consignes par rôle (`main`, `coder`, `researcher`, `reviewer`, `security`) |
| `settings.json` | paramètres du projet, prioritaires sur les globaux |
| `commands/<nom>.md` | commandes `/nom` du chat |
| `sonarqube.json` | serveur et jeton SonarQube du projet (extension SonarQube ; ignoré par git) |
| `chats/` | historique des conversations (ignoré par git). Pour un dossier qui n'est pas un projet, l'historique est gardé dans `<config>/chats/` |
