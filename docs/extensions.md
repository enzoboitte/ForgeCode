# Créer une extension

Une extension (« module ») est un dossier contenant un `plugin.toml`. Il n'y a rien à compiler : chaque action est une commande shell, écrite dans le langage de votre choix (sh, Python, Node, un binaire…).

Un module peut ajouter :

- des **commandes** (menu Modules, palette, lancement à l'enregistrement) ;
- des **outils** que les agents IA peuvent appeler ;
- des **langages** (extensions de fichier, coloration, serveur de langage, commande Exécuter) ;
- des **thèmes**.

## Où vivent les modules

| Système | Dossier |
|---|---|
| macOS | `~/Library/Application Support/forgecode/plugins/<nom>/` |
| Linux | `~/.config/forgecode/plugins/<nom>/` (ou `$XDG_CONFIG_HOME`) |
| Windows | `%APPDATA%\forgecode\plugins\<nom>\` |

Paramètres › Modules liste les modules chargés et les erreurs de `plugin.toml`. On y trouve **Nouveau module…** (aussi dans la palette, ⌘⇧P / Ctrl+Shift+P), qui crée un exemple commenté, et **Recharger les modules**, à utiliser après chaque modification.

## Marketplace

Paramètres › Modules › **Marketplace** affiche les extensions sous forme de cartes (icône, auteur, catégories, ce qu'elles apportent), avec une recherche et les filtres Toutes / Installées / Mises à jour. Il liste les extensions d'un dossier (par défaut `<config>/marketplace`, modifiable dans le même écran) : **Installer**, **Mettre à jour** (quand la `version` du marketplace est plus récente) et supprimer. Il n'y a pas encore de serveur : pour publier une extension, copiez son dossier dans le dossier du marketplace (un dossier partagé ou synchronisé convient pour une équipe). Une mise à jour copie les fichiers par-dessus l'installation, donc les fichiers créés par le module (sa configuration) sont gardés ; un fichier retiré d'une nouvelle version reste en place.

ForgeCode y dépose ses extensions intégrées, dont **SonarQube** (analyse avec un serveur distant ou un serveur local Docker créé automatiquement ; voir son `README.md`).

## Tutoriel : un module en 5 minutes

1. Palette › **Nouveau module…**. ForgeCode crée `plugins/my-module-1/` (avec un `plugin.toml` commenté, un script `hello.sh` et un dossier `themes/`) et ouvre le `plugin.toml`. Renommez le dossier si vous voulez.
2. Remplacez `plugin.toml` par :

   ```toml
   name = "outils-c"
   version = "0.1.0"
   description = "Formatage et comptage de lignes pour C/C++"

   [[commands]]
   title = "Formater avec clang-format"
   run = "clang-format -i \"${file}\" && echo '@forge {\"action\":\"reload\"}'"
   languages = ["c", "cpp"]
   on_save = true

   [[commands]]
   title = "Compter les lignes du projet"
   run = "compter.sh"
   ```

3. Créez `compter.sh` dans le dossier du module :

   ```sh
   #!/bin/sh
   n=$(git -C "$FORGE_WORKSPACE" ls-files | xargs cat 2>/dev/null | wc -l)
   echo "$n lignes"                                         # → panneau Sortie
   echo "@forge {\"action\":\"notify\",\"message\":\"$n lignes\"}"
   ```

   puis `chmod +x compter.sh` (macOS/Linux).
4. **Recharger les modules**. Les deux commandes apparaissent dans le menu Modules et dans la palette, et le formatage se lance à chaque enregistrement d'un fichier C/C++.

## Référence de `plugin.toml`

### En-tête

| Clé | Obligatoire | Rôle |
|---|---|---|
| `name` | oui | nom affiché |
| `version`, `description` | non | affichés dans le marketplace ; `version` décide des mises à jour |
| `author` | non | affiché sous le nom |
| `icon` | non | icône de l'extension : nom d'icône [Lucide](https://lucide.dev/icons) (`shield-check`, `database`…) ou fichier image du module (`icon.svg`) |
| `categories` | non | étiquettes affichées et cherchables dans le marketplace (`["Qualité", "Sécurité"]`) |
| `[activation] workspace_contains` | non | le module n'est actif que si l'un de ces fichiers existe à la racine du projet (`["package.json"]`, `["*.sql"]`) ; vide = tous les projets |
| `themes` | non | sous-dossier contenant des thèmes JSON |

### `[[commands]]`

| Clé | Obligatoire | Rôle |
|---|---|---|
| `title` | oui | libellé dans le menu Modules et la palette |
| `run` | oui | commande shell |
| `icon` | non | icône du bouton : nom Lucide (`play`, `bug`, `git-branch`…) ou fichier image du module |
| `placement` | non | où mettre son bouton : `menu` (menu Modules, par défaut), `title_bar` (barre de titre), `editor` (barre d'outils du fichier ouvert), `status_bar` (barre d'état). Plusieurs possibles : `["editor", "menu"]`. Toutes les commandes sont aussi dans la palette |
| `languages` | non | n'afficher et ne lancer la commande que pour ces langages (identifiants LSP : `c`, `cpp`, `rust`, `python`, `php`, `javascript`, `typescript`, `typescriptreact`, `html`, `css`, `json`, `sql`, `shellscript`…) |
| `on_save` | non | `true` : lancer après chaque enregistrement d'un fichier concerné |
| `on_open` | non | `true` : lancer à l'ouverture d'un fichier concerné |
| `on_startup` | non | `true` : lancer une fois à l'ouverture d'un projet où le module est actif |

Exemple : un bouton dans la barre de l'éditeur, seulement pour les fichiers SQL, dans les projets qui contiennent un `.sql` :

```toml
name = "outils-sql"
icon = "database"
[activation]
workspace_contains = ["*.sql"]

[[commands]]
title = "Formater le SQL"
run = "sqlfluff fix \"${file}\" && echo '@forge {\"action\":\"reload\"}'"
icon = "wand-sparkles"
placement = ["editor"]
languages = ["sql"]
```

### `[[tools]]` : outils pour l'IA

```toml
[[tools]]
name = "run_tests"                         # identifiant vu par l'agent
description = "Lance les tests du projet. Renvoie la sortie."
run = "cargo test $FORGE_ARG_filter"
[tools.parameters]                         # nom = description (toujours des chaînes)
filter = "Nom de test à filtrer (optionnel)"
```

L'agent lit `description` et la description des paramètres pour décider quand appeler l'outil : soyez précis. Il reçoit en réponse la sortie standard et la sortie d'erreur. Un outil est arrêté au bout de 5 minutes. En mode *Demander*, chaque appel attend votre validation.

### `[[languages]]` : nouveau langage

```toml
[[languages]]
extensions = ["zig", "zon"]      # sans le point
highlight = "zig"                # grammaire de coloration (liste ci-dessous)
lsp_id = "zig"                   # optionnel, défaut = highlight
server = ["zls"]                 # optionnel : serveur de langage et ses arguments
run = "zig run \"${file}\""      # optionnel : commande Exécuter (⌘R / Ctrl+R)
```

Grammaires de coloration intégrées : `bash c cmake cpp csharp css diff ejs elixir erb go graphql html java javascript json kotlin lua make markdown php proto python ruby rust scala sql svelte swift toml tsx typescript yaml zig`.

Le serveur doit parler LSP sur stdin/stdout. ForgeCode installe lui-même les serveurs qu'il connaît (voir le [guide](guide.md#langages)) ; ceux des modules doivent se trouver dans le `PATH` ou être donnés par chemin absolu. Une erreur de démarrage s'affiche au survol du statut du serveur, en bas à droite.

## Ce que reçoit une commande

La commande est lancée dans le dossier du projet, par `sh -c` (macOS, Linux) ou `cmd /C` (Windows). Le dossier du module est placé en tête du `PATH` : un script du module se lance par son nom (`run = "compter.sh"`).

| Motif dans `run` | Remplacé par |
|---|---|
| `${file}` | fichier actif (chemin absolu) |
| `${dir}` | dossier du fichier actif |
| `${stem}` | nom du fichier sans extension |
| `${out}` | exécutable temporaire `<tmp>/<stem>` (`.exe` sous Windows), pratique pour compiler puis lancer |
| `${workspace}` | dossier du projet |
| `${includes}` | options `-I` des dossiers d'en-têtes C/C++ du projet (`include`, `src`, `third_party/*/include`, vcpkg, Homebrew) |

| Variable d'environnement | Valeur |
|---|---|
| `FORGE_FILE`, `FORGE_WORKSPACE`, `FORGE_LANGUAGE` | fichier actif, projet, identifiant du langage |
| `FORGE_SELECTION` | texte sélectionné dans l'éditeur |
| `FORGE_PLUGIN_DIR` | dossier du module |
| `FORGE_ARGS` (JSON), `FORGE_ARG_<nom>` | arguments d'un outil IA |

Mettez les chemins entre guillemets (`"${file}"`) : ils peuvent contenir des espaces.

## Piloter l'IDE

La sortie d'une commande s'affiche dans le panneau Sortie. Une ligne qui commence par `@forge ` suivie d'un objet JSON est exécutée par l'IDE au lieu d'être affichée :

| Action | Effet |
|---|---|
| `{"action":"notify","message":"Terminé"}` | notification |
| `{"action":"open","path":"src/main.rs","line":10}` | ouvrir un fichier (relatif au projet), `line` facultatif |
| `{"action":"reload"}` | recharger le fichier actif depuis le disque |
| `{"action":"replace_buffer","text":"..."}` | remplacer le contenu du fichier actif |
| `{"action":"insert","text":"..."}` | insérer du texte au curseur |
| `{"action":"chat","message":"Explique ce fichier"}` | envoyer un message au chat IA |

Le JSON doit tenir sur une ligne. Depuis un script, laissez un outil faire l'échappement plutôt que de l'écrire à la main, par exemple en Python :

```python
#!/usr/bin/env python3
import json, os
text = open(os.environ["FORGE_FILE"]).read().upper()
print("@forge " + json.dumps({"action": "replace_buffer", "text": text}))
```

## Thèmes

Placez des fichiers JSON au format des thèmes [gpui-component](https://github.com/longbridge/gpui-component/tree/main/themes) dans le dossier indiqué par `themes`. Ils apparaissent dans Paramètres › Apparence, dans les listes des thèmes clair et sombre.

## Écrire un module multiplateforme

- `run` passe par le shell du système : `$FORGE_PLUGIN_DIR` fonctionne avec `sh`, mais sous Windows il faut `%FORGE_PLUGIN_DIR%`. Pour un module portable, mettez la logique dans un script Python ou Node et lancez-le par un petit lanceur de même nom, `outil` (sh, exécutable) et `outil.cmd` (Windows) : `run = "outil"` trouve le bon dans le `PATH`.
- Sous Windows, un script `.cmd` ou `.bat` placé dans le module se lance par son nom, comme un `.sh` ailleurs.
- Évitez les chemins en dur : utilisez `${workspace}`, `${file}` et les variables `FORGE_*`.

## Déboguer un module

- **Le module n'apparaît pas** : vérifiez Paramètres › Modules ; une erreur de syntaxe TOML y est affichée avec le fichier en cause. Le dossier doit contenir directement `plugin.toml`.
- **La commande ne fait rien** : sa sortie et ses erreurs sont dans le panneau Sortie (⌘J / Ctrl+J). Lancez-la dans le terminal intégré avec les mêmes variables pour la tester.
- **« permission denied »** sur un script : `chmod +x script.sh`.
- **Une ligne `@forge` s'affiche telle quelle** : le JSON est invalide ou ne tient pas sur une ligne ; le panneau Sortie indique l'erreur.
- **La commande n'est pas proposée** : `languages` ne contient pas le langage du fichier actif (`FORGE_LANGUAGE` donne l'identifiant attendu).

## Extensions par projet

Sans créer de module, un projet peut ajouter des commandes de chat dans `.forgecode/commands/<nom>.md` : le fichier est le prompt, `$ARGUMENTS` reçoit le texte tapé après `/nom`.

## Agents (`[agent]`)

Un module peut apporter un agent de code externe parlant l'Agent Client Protocol (ACP), comme **Hermes Agent** (fourni dans le marketplace). ForgeCode l'installe lui-même en arrière-plan à la première ouverture du panneau IA, dans le dossier `runtime/` du module (avec `uv` et son propre Python), puis le lance et remplace le chat par le panneau de l'agent (bouton pour revenir au chat classique).

```toml
[agent]
name = "Hermes"
package = "hermes-agent[acp] @ git+https://github.com/NousResearch/hermes-agent@v2026.9.24"
python = "3.14"
command = ["hermes", "acp"]
install_note = "Installation de Hermes…"
```

L'agent utilise les modèles de ForgeCode via un point d'accès local compatible OpenAI (`127.0.0.1`, clé secrète par lancement) : comptes GitHub Copilot et serveurs locaux, limités aux modèles cochés dans **Paramètres › IA › Agent Hermes**, avec la taille de contexte réglée là. Ses modifications de fichiers et commandes s'affichent comme des cartes à autoriser (avec le diff). La conversation du projet est reprise avec tout son contexte au redémarrage.
