# Bases de données

Une icône sous Contrôle de code source ouvre le panneau des bases :

- **SQLite** : les fichiers `.db`, `.sqlite`, `.sqlite3` du projet sont listés d'eux-mêmes ; un fichier s'ouvre aussi depuis l'explorateur.
- **Connexions** : PostgreSQL, MySQL, MariaDB, SQL Server, Oracle (hôte, port, utilisateur, base). Les mots de passe sont dans le trousseau macOS (sur les autres systèmes, dans `.forgecode/` du projet, ignoré par git). Option « lecture seule » par connexion.
- **Tables** : données en grille paginée, filtre `WHERE` et tri, modification d'une cellule, ajout et suppression de lignes, export CSV ; création d'une table et ajout / suppression / renommage de colonnes avec aperçu du SQL.
- **SQL** : une console par connexion (⌘↵ pour exécuter) ; toute requête qui modifie (`INSERT`, `UPDATE`, `DELETE`, `CREATE`, `ALTER`, `DROP`…) est confirmée avant.
- **Hermes** : il voit vos connexions (outils `db_list_connections`, `db_schema`, `db_query`) et peut proposer des modifications (`db_execute`) que vous acceptez ou refusez dans ForgeCode.

Les pilotes (psycopg, pymysql, pymssql, oracledb) sont installés par ForgeCode dans le dossier du module à la première connexion à un serveur ; SQLite n'en a pas besoin.
