# GamingRemoteExperience : consignes pour Claude

Clone de Parsec en Rust, sans serveur d'authentification. Voir `README.md`, `TUTO.md` et `docs/ARCHITECTURE.md`.

## Délégation au LLM local

Un LLM local (llama.cpp, Qwen3.6-35B-A3B, contexte 64k) sert à économiser des tokens.
**Cette règle s'applique à Claude ET à tous les agents et sous-agents** : tout prompt d'agent doit la reprendre.

```
python tools/local_llm.py --task "spec précise" --context fichier1 fichier2 --out chemin/du/fichier
```

**À DÉLÉGUER** : tests unitaires, boilerplate, fonctions isolées et bien spécifiées, scripts,
conversions de format, docstrings, CRUD simples.

**À GARDER POUR CLAUDE** : architecture, refactors multi-fichiers, debug complexe, sécurité,
concurrence, et tout ce qui demande de comprendre le projet globalement.

**Méthode**
1. Écrire une spec précise : signature, comportement, cas limites.
2. Passer les fichiers utiles via `--context` (pas de copier-coller dans `--task`).
3. Toujours utiliser `--out`. L'outil n'affiche qu'une ligne : chemin et nombre de lignes.
4. Lancer les tests et le linter (`cargo test -p <crate>`, `cargo clippy`). Ne relire le fichier en entier que si les tests échouent.
5. Après 2 échecs, écrire le code soi-même.

Remarques :
- `--out` refuse tout chemin hors du dépôt. Le fichier est **remplacé entièrement** : pour ajouter du code dans un fichier existant, faire écrire un nouveau fichier ou module, puis l'intégrer soi-même.
- Si `LOCAL_LLM_KEY` est défini, il est envoyé en `Authorization: Bearer`.
- Ne jamais passer en contexte de fichiers contenant des secrets : `identity.json`, `trust.json`, `hosts.json`, `status.json`, ou des jetons d'invitation.
