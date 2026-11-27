# CLAUDE.md

Guide pour travailler sur ce dépôt avec Claude Code. Documentation utilisateur : `README.md`.

## Le projet

`mcp-shell` : serveur MCP (HTTP streamable, TLS rustls) qui donne à un agent IA un accès shell
**restreint** à une machine Proxmox/Debian ou Alpine. Un binaire Rust statique (musl), deux outils
MCP : `get_policy` et `run_command`. C'est un composant de **sécurité** : un défaut ici donne un
shell root à un agent.

Langue du projet : **français** (code commenté, messages, logs, docs, commits).

## Architecture (`src/`)

| Fichier | Rôle |
|---|---|
| `main.rs` | CLI (`serve`, `gen-token`, `check`), routeur axum, TLS et rechargement du certificat, arrêt propre |
| `config.rs` | Chargement TOML (`deny_unknown_fields`) et validation ; `ExecConfig::for_tests()` pour les tests |
| `auth.rs` | Middleware : filtre IP puis jeton Bearer (empreintes SHA-256, comparaison en temps constant) |
| `policy.rs` | Liste noire, substitutions, redirections, `&`, découpage en segments, liste blanche |
| `exec.rs` | Exécution : env vidé, groupe de processus, échéance unique (attente + lecture), sortie plafonnée |
| `tools.rs` | Outils MCP (rmcp), journal d'audit de chaque appel |
| `audit.rs` | Journal JSON lines |

`packaging/` : configuration d'exemple, unités systemd/OpenRC, sudoers, logrotate. Ils sont
livrés dans l'archive de release : les garder cohérents avec le code.

`scripts/` : `package.sh` (archive de release), `install.sh` (installé à la racine de l'archive :
installation, mise à jour, désinstallation ; POSIX sh, dash et busybox), `test-install.sh`
(test de bout en bout d'`install.sh` dans Docker, Debian 12 et Alpine 3.20).

## Commandes

```bash
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
shellcheck -s sh scripts/*.sh
scripts/package.sh            # archive de release musl dans dist/ (musl-tools requis)
scripts/test-install.sh alpine:3.20 dist/mcp-shell-*-x86_64-linux.tar.gz   # Docker requis
```

Toujours `--locked` : `Cargo.lock` est versionné et fait foi. Version minimale de Rust :
`rust-version` dans `Cargo.toml` (vérifiée par la CI).

## Règles

- **Sécurité d'abord.** Toute modification de `policy.rs` ou `exec.rs` doit être accompagnée de
  tests, y compris de tests de contournement (ce qui doit être refusé). Le découpage de
  `split_segments` doit rester *plus strict* que bash, jamais plus permissif.
- Ne jamais élargir la liste blanche d'exemple avec des interpréteurs ou lanceurs génériques
  (`bash`, `sh`, `python`, `xargs`, `find -exec`, `pct exec`, `qm guest exec`…).
- sudoers : regex ancrées `^…$`, jamais de joker `*` dans les arguments (il accepte les espaces).
- Les tests de `exec.rs` lancent de vrais processus (`/bin/sh`, `sleep`, `setsid`, `pgrep`) : ils
  supposent Linux.
- Ne jamais committer de binaire, d'archive, de certificat ou de `config.toml` réel ; pas d'IP ou
  de nom d'hôte réels dans les exemples (le dépôt est public) : utiliser `203.0.113.0/24`,
  `exemple.fr`.
- Toute modification visible va dans `CHANGELOG.md` (section de la prochaine version), en
  signalant ce qu'un administrateur doit reporter sur ses serveurs (config, sudoers).
- `install.sh` ne doit jamais modifier une configuration ou un sudoers existants, ni démarrer
  le service à la première installation. Toute modification se teste avec `test-install.sh`
  sur les deux images.
- Une branche et une PR par changement ; la CI doit être verte.

## Publier une version

1. PR : `version` dans `Cargo.toml`, `cargo update -p mcp-shell --offline`, section datée dans
   `CHANGELOG.md`.
2. Après fusion : `git tag vX.Y.Z && git push origin vX.Y.Z`.
3. `.github/workflows/release.yml` vérifie tag/Cargo.toml/CHANGELOG, teste, construit l'archive,
   publie la Release (sha256 + attestation de provenance).
