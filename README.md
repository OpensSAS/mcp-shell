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
| `run_command` | Exécute une commande. Paramètres : `command`, `workdir` (optionnel, absolu), `timeout_secs` (optionnel). Renvoie `exit_code`, `stdout`, `stderr`, `truncated`, `timed_out`, `duration_ms`. Une commande refusée renvoie une erreur d'outil avec la raison. |

### Règles de filtrage

Dans l'ordre :

1. **IP** : hors `allowed_ips` → 403.
2. **Jeton** : absent ou inconnu → 401 (après 500 ms, contre la force brute).
3. **Liste noire** (`deny`) : regex cherchées dans la commande entière. Un motif trouvé = refus, quoi que dise la liste blanche.
4. **Substitutions** `$(…)`, `` `…` ``, `<(…)`, `>(…)` : toujours refusées.
5. **Redirections** `>` `<` : refusées sauf `allow_redirections = true`.
6. **Liste blanche** (`allow`) : la commande est découpée sur `|` `;` `&&` `||` `&` et retour ligne. **Chaque segment** doit correspondre **entièrement** à une regex (l'ancrage `^…$` est ajouté automatiquement).

Exemple avec `allow = ['sudo -n /usr/sbin/pct list', 'grep -i \w+']` :

| Commande | Résultat |
|---|---|
| `sudo -n /usr/sbin/pct list \| grep -i running` | autorisée |
| `sudo -n /usr/sbin/pct list; reboot` | refusée (`reboot`) |
| `sudo -n /usr/sbin/pct list $(id)` | refusée (substitution) |

Le découpage ignore les guillemets : il ne peut que rendre la vérification plus stricte.

### Exécution

- Shell lancé avec un environnement vidé (`PATH` fixé par la config, `LANG=C.UTF-8`), stdin fermé.
- **Timeout** : tout le groupe de processus est tué (le shell et ses enfants).
- **Sortie** plafonnée à `max_output_bytes` par flux ; le reste est lu et jeté.

## 2. Installation

Les commandes ci-dessous supposent le binaire copié dans `/tmp/mcp-shell`.

### 2.1 Sur un hôte Proxmox (Debian, systemd)

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

### 2.2 Sur Alpine (OpenRC)

```bash
apk add bash sudo
addgroup -S mcp-shell && adduser -S -D -H -h /var/lib/mcp-shell -s /sbin/nologin -G mcp-shell mcp-shell

install -m 0755 /tmp/mcp-shell /usr/local/bin/mcp-shell
install -d -m 0750 -g mcp-shell /etc/mcp-shell
install -m 0640 -g mcp-shell config.example.toml /etc/mcp-shell/config.toml
install -m 0440 sudoers.mcp-shell /etc/sudoers.d/mcp-shell   # adapter les chemins
install -m 0755 mcp-shell.openrc /etc/init.d/mcp-shell
rc-update add mcp-shell default
```

Le binaire est lié statiquement : il tourne tel quel sur Alpine (musl) comme sur Debian.

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

La commande affiche le jeton (à donner au client, il n'est stocké nulle part) et le bloc à ajouter :

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
- N'autorisez jamais un interpréteur ou un lanceur générique : `bash`, `sh`, `python`, `perl`, `eval`, `xargs`, `find -exec`, `pct exec`, `pct enter`, `qm guest exec`. Chacun contourne la liste blanche.
- Gardez une liste noire de sécurité (`rm`, `dd`, `mkfs`, `reboot`, `curl`, `wget`…) même si la liste blanche ne les autorise pas : elle protège contre une regex trop large ajoutée plus tard.

### Passer aux actions (phase 2)

Ajoutez une à une les commandes d'action, par exemple :

```toml
'sudo -n /usr/sbin/pct snapshot \d+ agent-\d{8}-\d{4}',
'sudo -n /usr/sbin/pct start \d+',
```

avec les entrées sudoers correspondantes. Côté Paperclip, exigez une approbation avant les tâches qui les utilisent.

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

```
/var/log/mcp-shell/audit.jsonl {
    weekly
    rotate 12
    compress
    missingok
    notifempty
    create 0640 mcp-shell mcp-shell
}
```

À placer dans `/etc/logrotate.d/mcp-shell`.

### Modifier la configuration

```bash
mcp-shell check -c /etc/mcp-shell/config.toml && systemctl restart mcp-shell
```

### Mettre à jour le binaire

```bash
install -m 0755 /tmp/mcp-shell /usr/local/bin/mcp-shell && systemctl restart mcp-shell
```

## 8. Dépannage

| Symptôme | Cause probable | Action |
|---|---|---|
| Le service refuse de démarrer : « pas de section [tls] » | Ni `[tls]` ni `allow_plain_http` | Ajouter le certificat, ou passer derrière un proxy local |
| « chargement TLS » au démarrage | Chemin faux ou fichiers illisibles par `mcp-shell` | `sudo -u mcp-shell cat /etc/mcp-shell/privkey.pem >/dev/null` |
| Client : 403 | IP hors `allowed_ips` | Vérifier l'IP vue dans `journalctl -u mcp-shell` |
| Client : 401 | Jeton faux, ou en-tête sans `Bearer ` | Regénérer un jeton, vérifier `claude mcp list` |
| Client : erreur sur l'en-tête Host | Nom utilisé absent de `allowed_hosts` | Ajouter le nom, avec et sans port |
| Client : erreur de certificat | Certificat auto-signé ou nom différent | Certificat Let's Encrypt au bon nom |
| `sudo: a password is required` | Commande absente de sudoers ou arguments différents | Aligner `/etc/sudoers.d/mcp-shell` sur `allow`, `visudo -cf` |
| `journalctl` vide ou refusé | Utilisateur hors du groupe `systemd-journal` | `usermod -aG systemd-journal mcp-shell`, redémarrer le service |
| `timed_out: true` | Commande plus longue que le timeout | Passer `timeout_secs` (plafonné par `max_timeout_secs`) |
| `truncated: true` | Sortie au-delà de `max_output_bytes` | Filtrer (`\| tail -n 50`) ou augmenter la limite |
| Avertissement « le service tourne en root » | Service lancé sans `User=` | Toujours utiliser l'utilisateur `mcp-shell` |

## 9. Compilation

Toolchain Rust ≥ 1.88.

```bash
# Binaire statique (glibc), portable Debian/Proxmox et Alpine
RUSTFLAGS="-C target-feature=+crt-static" \
  cargo build --release --target x86_64-unknown-linux-gnu
# → target/x86_64-unknown-linux-gnu/release/mcp-shell

cargo test        # tests de la politique de filtrage
```

Dépendances principales : `rmcp` (SDK MCP officiel), `axum`, `axum-server` + `rustls` (ring), `regex`, `sha2`, `subtle`.
