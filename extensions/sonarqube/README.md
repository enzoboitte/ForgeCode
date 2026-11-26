# SonarQube pour ForgeCode

Analyse la qualité et la sécurité du projet avec SonarQube, sur un serveur existant ou sur un serveur local que le module crée avec Docker.

## Prérequis

- Python 3.8 ou plus récent (`python3` sous macOS et Linux, `py` ou `python` sous Windows).
- Pour le serveur local : Docker Desktop (macOS, Windows) ou Docker Engine avec Compose (Linux).
- Pour l'analyse : `sonar-scanner` s'il est installé, sinon l'image Docker `sonarsource/sonar-scanner-cli` est utilisée.

## Démarrage

**Serveur local** : palette › « SonarQube : démarrer un serveur local (Docker) ». Le module lance SonarQube Community et PostgreSQL (`docker-compose.yml`), attend que le serveur soit prêt, remplace le mot de passe `admin/admin` par un mot de passe aléatoire et crée un jeton. La configuration du projet (adresse, jeton) est enregistrée dans `.forgecode/sonarqube.json`, ajouté automatiquement au `.gitignore` de `.forgecode` ; le mot de passe admin du serveur local, partagé par tous les projets, reste dans `local-server.json`, dans le dossier du module. Les données restent dans des volumes Docker ; « arrêter le serveur local » ne les supprime pas.

**Serveur distant ou SonarCloud** : « SonarQube : se connecter à un serveur… » ouvre `.forgecode/sonarqube.json` ; renseignez `url`, `token` (Mon compte › Sécurité) et, pour SonarCloud, `organization`. Les variables `SONAR_HOST_URL` et `SONAR_TOKEN` remplacent le fichier.

## Commandes

| Commande | Effet |
|---|---|
| Analyser le projet | lance le scanner, attend le traitement, affiche la quality gate, les mesures et tous les problèmes (`fichier:ligne: SÉVÉRITÉ [type] message (règle)`) |
| Problèmes du fichier courant | problèmes ouverts du fichier actif |
| Corriger le fichier courant avec l'IA | envoie les problèmes du fichier au chat |
| Ouvrir le tableau de bord | page du projet dans le navigateur |
| État | serveur, état, validité du jeton, clé du projet |

Outils pour l'IA : `sonarqube_issues` (filtres `file`, `severity`), `sonarqube_rule` (explication d'une règle), `sonarqube_analyze`.

## Projet

La clé du projet est le nom du dossier, ou `sonar.projectKey` de `sonar-project.properties` ; si ce fichier existe, ses réglages remplacent ceux du module. `node_modules`, `target`, `build`, `dist`, `vendor`, `.venv` sont exclus. Ajoutez `.scannerwork/` à votre `.gitignore`.

L'édition Community n'analyse pas le C et le C++ (édition Developer requise).
