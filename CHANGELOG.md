# Changelog

Format : [Keep a Changelog](https://keepachangelog.com/fr/1.1.0/), versions [SemVer](https://semver.org/lang/fr/).
Chaque version publiée correspond à un tag `vX.Y.Z` et à une Release GitHub construite par la CI.

## [0.1.3] - 2026-11-27

### Ajouté
- **Script d'installation `install.sh`**, livré à la racine de l'archive de release
  (Debian/Proxmox avec systemd, Alpine avec OpenRC) :
  - première installation : utilisateur, répertoires et droits, binaire, configuration avec
    un jeton neuf (affiché une fois, avec la commande `claude mcp add`), sudoers validé par
    `visudo -cf`, service, rotation des journaux ; le service n'est pas démarré et la liste de
    ce qui reste à faire est affichée (certificat, liste blanche, démarrage) ;
  - mise à jour : configuration existante vérifiée par la nouvelle version avant toute
    modification ; binaire et service remplacés ; redémarrage si le service tournait ;
    configuration et sudoers existants jamais modifiés (différence sudoers affichée) ;
  - options `--host`, `--allow-ip`, `--client`, `--server-name`, `--pve-cert` (certificat
    ACME de Proxmox), `--dry-run`, `--uninstall [--purge]`.
- `packaging/mcp-shell.logrotate`.
- CI : shellcheck, et test d'installation de bout en bout dans des conteneurs Debian 12 et
  Alpine 3.20 (aussi exécuté avant chaque release).

## [0.1.2] - 2026-11-27

### Sécurité
- **Timeout contournable** : le timeout ne couvrait que l'attente du shell, pas la lecture
  de sa sortie. Une commande mise en arrière-plan (`sleep 3600 &`) gardait stdout ouvert :
  la requête restait bloquée jusqu'à la fin du processus, avec `timed_out: false`, et le
  processus survivait à la requête. Désormais une échéance unique couvre toute l'exécution,
  et le groupe de processus est tué à la fin même si le shell s'est terminé normalement.
- **Mise en arrière-plan refusée** : `&` seul est refusé par la politique (`&&`, `|&`,
  `2>&1`, `&>` restent acceptés).
- **sudoers** : l'exemple utilisait des jokers (`pct config [0-9]*`) ; dans sudoers `*`
  accepte aussi les espaces, donc des options supplémentaires. Remplacés par des regex
  ancrées `^…$` (sudo ≥ 1.9.10). **À reporter dans `/etc/sudoers.d/mcp-shell`.**

### Modifié
- Le schéma `Bearer` de l'en-tête `Authorization` est insensible à la casse (RFC 9110).
- Binaire de release lié statiquement avec musl (au lieu de glibc statique).
- Version minimale de Rust : 1.89 (déclarée dans `Cargo.toml`, vérifiée par la CI).

### Projet
- `Cargo.lock` versionné ; archives binaires retirées du dépôt (publiées en Releases).
- CI GitHub Actions : format, clippy, tests, version minimale de Rust, build musl.
- Release automatique sur tag `v*`, avec sha256 et attestation de provenance.
- Tests : 4 → 15 (exécution, timeout, arrière-plan, troncature, environnement, jetons).
- `LICENSE` (MIT), `CHANGELOG.md`, `CLAUDE.md`, Dependabot.

## [0.1.1] - 2026-11-25

### Ajouté
- Annotations MCP : `run_command` déclaré en écriture et destructif, `get_policy` en lecture seule.

### Modifié
- Le service refuse de démarrer tant qu'une empreinte de jeton vaut `000…0` (valeur d'exemple).
- Documentation : `gen-token`, annotations, dépannage.

## [0.1.0] - 2026-11-20

Première version : serveur MCP HTTP streamable avec TLS (rechargement à chaud), jetons Bearer
(empreintes SHA-256), filtrage IP et `Host`, liste blanche/noire par segment, exécution avec
timeout et sortie plafonnée, journal d'audit JSON lines, sous-commandes `serve`, `gen-token`,
`check`, fichiers systemd/OpenRC/sudoers.

[0.1.3]: https://github.com/openssas/mcp-shell/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/openssas/mcp-shell/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/openssas/mcp-shell/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/openssas/mcp-shell/releases/tag/v0.1.0
