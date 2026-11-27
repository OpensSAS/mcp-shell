# mcp-shell

Serveur MCP qui donne à un agent IA un accès shell **restreint** à une machine : HTTPS natif, authentification par jeton, liste blanche de commandes, journal d'audit. Un binaire statique unique, sans dépendance, à installer sur chaque serveur à maintenir.

## 1. Présentation

### Principe

```
Agent (Paperclip / Claude Code)
        │  HTTPS + Authorization: Bearer <jeton>
        ▼
mcp-shell (sur le serveur cible, utilisateur mcp-shell)
        │  filtre IP → jeton → liste noire → liste blanche
        ▼
/bin/bash -c "<commande>"   (sudo -n pour les commandes root déclarées)
```

L'agent ne détient ni clé SSH ni mot de passe : seulement l'URL et un jeton. Il ne peut exécuter que ce que la configuration autorise, et chaque appel est journalisé.

### Outils MCP exposés

| Outil | Rôle |
|---|---|
| `get_policy` | Renvoie le nom du serveur, les regex autorisées et interdites, les limites. L'agent l'appelle avant d'agir. |
| `run_command` | Exécute une commande (déclarée en écriture et destructive dans les annotations MCP : les clients peuvent exiger une approbation). Paramètres : `command`, `workdir` (optionnel, absolu), `timeout_secs` (optionnel). Renvoie `exit_code`, `stdout`, `stderr`, `truncated`, `timed_out`, `duration_ms`. Une commande refusée renvoie une erreur d'outil avec la raison. |

### Règles de filtrage

Dans l'ordre :

1. **IP** : hors `allowed_ips` → 403.
2. **Jeton** : absent ou inconnu → 401 (après 500 ms, contre la force brute).
3. **Liste noire** (`deny`) : regex cherchées dans la commande entière. Un motif trouvé = refus, quoi que dise la liste blanche.
4. **Substitutions** `$(…)`, `` `…` ``, `<(…)`, `>(…)` : toujours refusées.
5. **Redirections** `>` `<` : refusées sauf `allow_redirections = true`.
6. **Mise en arrière-plan** `&` : toujours refusée (`&&`, `|&`, `2>&1` restent possibles).
7. **Liste blanche** (`allow`) : la commande est découpée sur `|` `|&` `;` `&&` `||` et retour ligne. **Chaque segment** doit correspondre **entièrement** à une regex (l'ancrage `^…$` est ajouté automatiquement).

Exemple avec `allow = ['sudo -n /usr/sbin/pct list', 'grep -i \w+']` :

| Commande | Résultat |
|---|---|
| `sudo -n /usr/sbin/pct list \| grep -i running` | autorisée |
| `sudo -n /usr/sbin/pct list; reboot` | refusée (`reboot`) |
| `sudo -n /usr/sbin/pct list $(id)` | refusée (substitution) |
| `sudo -n /usr/sbin/pct list &` | refusée (arrière-plan) |

Le découpage ignore les guillemets : il ne peut que rendre la vérification plus stricte.

### Exécution

- Shell lancé avec un environnement vidé (`PATH` fixé par la config, `LANG=C.UTF-8`), stdin fermé.
- **Timeout** : une échéance unique couvre l'exécution et la lecture de la sortie. À l'échéance, ou dès que le shell se termine, tout le groupe de processus est tué (le shell et ses enfants) : rien ne survit à la requête.
- **Sortie** plafonnée à `max_output_bytes` par flux ; le reste est lu et jeté.

## 2. Installation

### Récupérer et vérifier une version

Les binaires sont publiés dans les [Releases GitHub](https://github.com/openssas/mcp-shell/releases), construits par la CI à partir du tag (binaire statique musl, Debian/Proxmox et Alpine).

```bash
V=0.1.3
curl -fsSLO https://github.com/openssas/mcp-shell/releases/download/v$V/mcp-shell-$V-x86_64-linux.tar.gz
curl -fsSLO https://github.com/openssas/mcp-shell/releases/download/v$V/mcp-shell-$V-x86_64-linux.tar.gz.sha256
sha256sum -c mcp-shell-$V-x86_64-linux.tar.gz.sha256
# Optionnel, depuis un poste avec gh : prouve que l'archive a été construite par la CI du dépôt
gh attestation verify mcp-shell-$V-x86_64-linux.tar.gz -R openssas/mcp-shell
tar xzf mcp-shell-$V-x86_64-linux.tar.gz && cd mcp-shell-$V
```

### Installation par script (recommandé)

Depuis l'archive extraite, en root. Prérequis : `sudo` ≥ 1.9.10 et `bash` (Alpine : `apk add bash sudo`).

```bash
./install.sh --dry-run                       # voir ce qui serait fait
./install.sh --host mcp-pve.exemple.fr --allow-ip 203.0.113.10/32
```

| Option | Rôle |
|---|---|
| `--host NOM` | nom DNS utilisé par le client (`allowed_hosts`) |
| `--allow-ip CIDR` | réseau autorisé (répétable) |
| `--client NOM` | nom du client du premier jeton (défaut `paperclip`) |
| `--server-name NOM` | nom annoncé à l'agent (défaut : nom de la machine) |
| `--pve-cert` | Proxmox : copie quotidienne du certificat ACME de pveproxy (§3) |
| `--init systemd\|openrc\|none` | forcer le gestionnaire de services (détecté sinon) |
| `--dry-run` | simulation, rien n'est modifié |
| `--uninstall [--purge]` | désinstaller (`--purge` : aussi configuration, journaux, utilisateur) |

**Première installation** : le script crée l'utilisateur `mcp-shell`, les répertoires et leurs droits, installe le binaire, génère la configuration **avec un jeton neuf** (affiché une seule fois, avec la commande `claude mcp add` à lancer côté client), installe sudoers après validation par `visudo -cf`, le service (systemd ou OpenRC) et la rotation des journaux. Il **ne démarre pas** le service : il affiche ce qui reste à faire (certificat TLS, relecture de la liste blanche, démarrage).

**Mise à jour** : relancer `./install.sh` depuis la nouvelle archive. La nouvelle version vérifie d'abord la configuration existante (sinon arrêt, rien n'est modifié), remplace binaire et service, redémarre le service s'il tournait. La configuration et le sudoers existants ne sont **jamais** modifiés ; si le sudoers livré diffère, la différence est affichée.

Le script est testé à chaque modification dans des conteneurs Debian 12 et Alpine 3.20 (installation, appels MCP réels via sudo, mise à jour, désinstallation).

### Installation manuelle

Détail de ce que fait le script, pour une installation à la main. Les commandes supposent le binaire copié dans `/tmp/mcp-shell` et les fichiers de `packaging/` dans le répertoire courant :

```bash
cp bin/mcp-shell /tmp/mcp-shell && cp packaging/* .
```

#### Sur un hôte Proxmox (Debian, systemd)

```bash
# Utilisateur dédié, sans shell de connexion
useradd --system --home-dir /var/lib/mcp-shell --create-home --shell /usr/sbin/nologin mcp-shell
usermod -aG systemd-journal mcp-shell        # pour journalctl

# Binaire
install -m 0755 /tmp/mcp-shell /usr/local/bin/mcp-shell
mcp-shell --version

# Configuration
install -d -m 0750 -g mcp-shell /etc/mcp-shell
install -m 0640 -g mcp-shell config.example.toml /etc/mcp-shell/config.toml
install -d -m 0750 -o mcp-shell -g mcp-shell /var/log/mcp-shell

# Droits root limités
install -m 0440 sudoers.mcp-shell /etc/sudoers.d/mcp-shell
visudo -cf /etc/sudoers.d/mcp-shell

# Service
install -m 0644 mcp-shell.service /etc/systemd/system/mcp-shell.service
systemctl daemon-reload
```

Ne démarrez le service qu'après les étapes 3 (certificat) et 4 (jeton).

#### Sur Alpine (OpenRC)

```bash
apk add bash sudo
addgroup -S mcp-shell && adduser -S -D -H -h /var/lib/mcp-shell -s /sbin/nologin -G mcp-shell mcp-shell

install -m 0755 /tmp/mcp-shell /usr/local/bin/mcp-shell
install -d -m 0750 -g mcp-shell /etc/mcp-shell
install -m 0640 -g mcp-shell config.example.toml /etc/mcp-shell/config.toml
install -m 0440 sudoers.mcp-shell /etc/sudoers.d/mcp-shell   # adapter les chemins (sudo >= 1.9.10)
install -m 0755 mcp-shell.openrc /etc/init.d/mcp-shell
rc-update add mcp-shell default
```

Le binaire est lié statiquement (musl) : il tourne tel quel sur Alpine comme sur Debian.

## 3. Certificat TLS

Le client (Claude Code dans Paperclip) doit faire confiance au certificat : utilisez un certificat Let's Encrypt pour le nom déclaré dans `allowed_hosts`.

Le service relit le certificat dès que le fichier change (vérification toutes les `reload_interval_secs`), sans redémarrage.

**Réutiliser le certificat ACME de Proxmox** (Datacenter → ACME) : copiez-le après chaque renouvellement plutôt que d'ouvrir `/etc/pve` à l'utilisateur `mcp-shell`.

```bash
cat > /etc/cron.daily/mcp-shell-cert <<'EOF'
#!/bin/sh
install -m 0640 -g mcp-shell /etc/pve/local/pveproxy-ssl.pem /etc/mcp-shell/fullchain.pem
install -m 0640 -g mcp-shell /etc/pve/local/pveproxy-ssl.key /etc/mcp-shell/privkey.pem
EOF
chmod 0755 /etc/cron.daily/mcp-shell-cert && /etc/cron.daily/mcp-shell-cert
```

**Derrière un reverse proxy** : supprimez `[tls]`, mettez `allow_plain_http = true` et `listen = "127.0.0.1:8443"`. Le filtrage `allowed_ips` voit alors l'IP du proxy : filtrez les IP au niveau du proxy.

## 4. Jetons

```bash
mcp-shell gen-token
```

La commande **n'écrit rien** : elle affiche le jeton (à donner au client, il n'est stocké nulle part) et le bloc à recopier dans la configuration, à la place de l'exemple. Le service refuse de démarrer tant qu'une empreinte vaut encore `000…0`.

```toml
[[tokens]]
name = "paperclip"
sha256 = "…"
```

Un bloc par client, avec un `name` distinct : il apparaît dans l'audit et permet de révoquer un client seul (supprimer son bloc, redémarrer).

## 5. Liste blanche

### Régler et tester

```bash
mcp-shell check -c /etc/mcp-shell/config.toml
mcp-shell check -c /etc/mcp-shell/config.toml --command "sudo -n /usr/sbin/pct list | grep -i running"
```

`check` valide la configuration et indique si une commande serait acceptée, sans l'exécuter. Code retour 2 si refusée.

### Bonnes pratiques

- Écrivez des regex **étroites** : `pct config \d+` plutôt que `pct .*`.
- Les commandes root passent par `sudo -n /chemin/absolu` et doivent figurer **à l'identique** dans `/etc/sudoers.d/mcp-shell`. La liste blanche filtre ce que l'agent demande ; sudoers limite ce que le système accepte.
- Dans sudoers, n'utilisez **pas** le joker `*` dans les arguments : il accepte aussi les espaces, donc des options en plus (`pct config [0-9]*` laisse passer `pct config 100 --current`). Utilisez une regex ancrée, équivalente à celle de `allow` : `/usr/sbin/pct ^config [0-9]+$` (sudo ≥ 1.9.10 : Debian 12 / Proxmox 8, Alpine 3.16+). Testez avec `sudo -l -U mcp-shell /usr/sbin/pct config 100`.
- N'autorisez jamais un interpréteur ou un lanceur générique : `bash`, `sh`, `python`, `perl`, `eval`, `xargs`, `find -exec`, `pct exec`, `pct enter`, `qm guest exec`. Chacun contourne la liste blanche.
- Gardez une liste noire de sécurité (`rm`, `dd`, `mkfs`, `reboot`, `curl`, `wget`…) même si la liste blanche ne les autorise pas : elle protège contre une regex trop large ajoutée plus tard.

### Passer aux actions (phase 2)

Ajoutez une à une les commandes d'action, par exemple :

```toml
'sudo -n /usr/sbin/pct snapshot \d+ agent-\d{8}-\d{4}',
'sudo -n /usr/sbin/pct start \d+',
```

avec les entrées sudoers correspondantes, par exemple :

```
/usr/sbin/pct ^snapshot [0-9]+ agent-[0-9]{8}-[0-9]{4}$, /usr/sbin/pct ^start [0-9]+$
```

Côté Paperclip, exigez une approbation avant les tâches qui les utilisent.

## 6. Démarrage et branchement Paperclip

### Démarrer

```bash
systemctl enable --now mcp-shell     # Proxmox
rc-service mcp-shell start           # Alpine
curl -s https://mcp-pve.exemple.fr:8443/health          # → ok
```

### Pare-feu

Ouvrez le port d'écoute **uniquement** depuis l'IP du VPS Paperclip (pfSense ou pare-feu Proxmox). `allowed_ips` est une seconde barrière, pas la première.

### Déclarer le serveur dans Paperclip

Dans le conteneur Paperclip, dans le répertoire de travail de l'agent concerné (un répertoire par agent pour que lui seul voie ce serveur) :

```bash
claude mcp add --transport http --scope project pve-opens \
  https://mcp-pve.exemple.fr:8443/mcp \
  --header "Authorization: Bearer <jeton>"
```

Un serveur = une entrée (`pve-opens`, `pve-qualitia`…). Dans les instructions de l'agent, indiquez : « appelle `get_policy` avant toute commande ; si une commande est refusée, ne cherche pas à la contourner, ouvre un ticket ».

## 7. Exploitation

### Journaux

| Quoi | Où |
|---|---|
| Service (démarrage, refus, erreurs) | `journalctl -u mcp-shell` (systemd) ou `/var/log/mcp-shell/service.log` (OpenRC) |
| Audit (un appel = une ligne JSON) | `/var/log/mcp-shell/audit.jsonl` |

```bash
# Commandes refusées du jour
grep '"outcome":"denied"' /var/log/mcp-shell/audit.jsonl | grep "$(date -u +%F)"
# Activité d'un client
grep '"client":"paperclip"' /var/log/mcp-shell/audit.jsonl | tail -n 20
```

Niveau de détail : variable `RUST_LOG` (ex. `RUST_LOG=mcp_shell=debug`) dans le service.

### Rotation de l'audit

Le fichier est rouvert à chaque écriture : une rotation simple suffit.

Fichier `packaging/mcp-shell.logrotate`, installé par `install.sh` dans `/etc/logrotate.d/mcp-shell` (rotation hebdomadaire, 12 semaines conservées, compression).

### Modifier la configuration

```bash
mcp-shell check -c /etc/mcp-shell/config.toml && systemctl restart mcp-shell
```

### Mettre à jour le binaire

Lisez d'abord le [CHANGELOG](CHANGELOG.md) : certaines versions demandent d'adapter la configuration ou le fichier sudoers.

```bash
tar xzf mcp-shell-X.Y.Z-x86_64-linux.tar.gz && cd mcp-shell-X.Y.Z && ./install.sh
```

À la main : `install -m 0755 /tmp/mcp-shell /usr/local/bin/mcp-shell && systemctl restart mcp-shell`.

## 8. Dépannage

| Symptôme | Cause probable | Action |
|---|---|---|
| Le service refuse de démarrer : « pas de section [tls] » | Ni `[tls]` ni `allow_plain_http` | Ajouter le certificat, ou passer derrière un proxy local |
| « chargement TLS » au démarrage | Chemin faux ou fichiers illisibles par `mcp-shell` | `sudo -u mcp-shell cat /etc/mcp-shell/privkey.pem >/dev/null` |
| Client : 403 | IP hors `allowed_ips` | Vérifier l'IP vue dans `journalctl -u mcp-shell` |
| Client : 401 | Jeton faux, ou en-tête sans `Bearer ` | Regénérer un jeton, vérifier `claude mcp list` |
| « l'empreinte vaut encore 000…0 » au démarrage | Empreinte d'exemple non remplacée | `mcp-shell gen-token`, coller la valeur `sha256` dans `[[tokens]]` |
| Client : erreur sur l'en-tête Host | Nom utilisé absent de `allowed_hosts` | Ajouter le nom, avec et sans port |
| Client : erreur de certificat | Certificat auto-signé ou nom différent | Certificat Let's Encrypt au bon nom |
| `sudo: a password is required` | Commande absente de sudoers ou arguments différents | Aligner `/etc/sudoers.d/mcp-shell` sur `allow`, `visudo -cf` |
| `journalctl` vide ou refusé | Utilisateur hors du groupe `systemd-journal` | `usermod -aG systemd-journal mcp-shell`, redémarrer le service |
| `timed_out: true` | Commande plus longue que le timeout | Passer `timeout_secs` (plafonné par `max_timeout_secs`) |
| `truncated: true` | Sortie au-delà de `max_output_bytes` | Filtrer (`\| tail -n 50`) ou augmenter la limite |
| Avertissement « le service tourne en root » | Service lancé sans `User=` | Toujours utiliser l'utilisateur `mcp-shell` |

## 9. Développement

Toolchain Rust ≥ 1.89 (champ `rust-version` de `Cargo.toml`, vérifié par la CI).

```bash
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked          # politique, exécution (timeout, arrière-plan…), jetons
shellcheck -s sh scripts/*.sh
```

### Tester le script d'installation

```bash
scripts/package.sh
scripts/test-install.sh debian:12   dist/mcp-shell-*-x86_64-linux.tar.gz
scripts/test-install.sh alpine:3.20 dist/mcp-shell-*-x86_64-linux.tar.gz
```

Docker requis. Dans un conteneur jetable : simulation, installation, serveur démarré avec un certificat auto-signé, appels MCP réels (dont une commande via `sudo` contre de faux `pct`/`qm`), relance sans écrasement, désinstallation, purge.

### Binaire de release en local

```bash
rustup target add x86_64-unknown-linux-musl
sudo apt-get install musl-tools          # fournit musl-gcc (pour ring)
scripts/package.sh
# → dist/mcp-shell-<version>-x86_64-linux.tar.gz et son .sha256
```

### Contribuer

- Une branche et une pull request par changement : la CI (`.github/workflows/ci.yml`) lance format, clippy, tests, vérification de la version minimale de Rust et build musl.
- Ne committez ni binaire ni archive : ils sont produits par la CI.
- Toute modification visible est notée dans [CHANGELOG.md](CHANGELOG.md), section de la prochaine version.

### Publier une version

1. Dans une PR : passer `version` dans `Cargo.toml` (puis `cargo update -p mcp-shell --offline` pour `Cargo.lock`), dater la section du `CHANGELOG.md`.
2. Après fusion dans `main` :
   ```bash
   git checkout main && git pull
   git tag v0.1.2 && git push origin v0.1.2
   ```
3. Le workflow `release.yml` vérifie que le tag correspond à `Cargo.toml` et au CHANGELOG, teste, construit l'archive, publie la Release avec sha256 et attestation de provenance.

Dépendances principales : `rmcp` (SDK MCP officiel), `axum`, `axum-server` + `rustls` (ring), `regex`, `sha2`, `subtle`.
