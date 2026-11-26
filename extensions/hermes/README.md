# Hermes Agent

[Hermes Agent](https://github.com/NousResearch/hermes-agent) (Nous Research, licence MIT) dans ForgeCode.

- **Installation** : installer le module depuis Paramètres › Modules › Marketplace. ForgeCode installe Hermes en arrière-plan dans le dossier du module (`runtime/`, avec son propre Python via uv). Supprimer le module supprime Hermes.
- **Modèles** : Hermes passe par ForgeCode (un point d'accès local, compatible OpenAI) et n'utilise que les modèles cochés dans Paramètres › IA › Agent : comptes GitHub Copilot et serveurs locaux (LM Studio, Ollama…).
- **Contexte** : la taille de contexte réglée dans Paramètres › IA › Agent ; au-delà, Hermes résume les anciens échanges. Une conversation reprise garde tout son contexte.
- **Données** : mémoire, compétences et sessions de Hermes sont dans le dossier de configuration de ForgeCode (`agents/hermes`), pas dans `~/.hermes`.
- **Panneau IA** : bouton Hermes / Chat pour basculer entre Hermes et le chat classique.
