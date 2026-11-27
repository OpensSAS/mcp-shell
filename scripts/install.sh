#!/bin/sh
# Installation, mise à jour et désinstallation de mcp-shell.
#
# À lancer en root depuis l'archive de release extraite :
#   tar xzf mcp-shell-X.Y.Z-x86_64-linux.tar.gz && cd mcp-shell-X.Y.Z && ./install.sh
#
# Première installation : utilisateur, binaire, configuration avec un jeton neuf, sudoers,
# service. Le service n'est PAS démarré : il reste à fournir le certificat TLS et à relire
# la configuration (liste affichée à la fin).
# Mise à jour : remplace le binaire et le service, redémarre le service s'il tournait.
# Ne modifie jamais une configuration ni un sudoers existants.
#
# POSIX sh : fonctionne avec dash (Debian/Proxmox) et ash/busybox (Alpine).
set -eu

# --- Emplacements -------------------------------------------------------------------------

SRC_DIR=$(cd "$(dirname "$0")" && pwd)
NEW_BIN="$SRC_DIR/bin/mcp-shell"
PKG="$SRC_DIR/packaging"

USER_NAME=mcp-shell
BIN=/usr/local/bin/mcp-shell
ETC=/etc/mcp-shell
CONFIG=$ETC/config.toml
LOG_DIR=/var/log/mcp-shell
HOME_DIR=/var/lib/mcp-shell
SUDOERS=/etc/sudoers.d/mcp-shell
UNIT=/etc/systemd/system/mcp-shell.service
INITD=/etc/init.d/mcp-shell
LOGROTATE=/etc/logrotate.d/mcp-shell
PVE_CERT_CRON=/etc/cron.daily/mcp-shell-cert

# --- Options ------------------------------------------------------------------------------

DRY_RUN=0
ACTION=install
PURGE=0
INIT=auto
CLIENT=paperclip
SERVER_NAME=$(hostname 2>/dev/null || echo serveur)
HOST=
ALLOW_IPS=
PVE_CERT=0
RESTART=1

usage() {
    cat <<'EOF'
Usage : ./install.sh [options]

  --dry-run             affiche ce qui serait fait, sans rien modifier
  --client NOM          nom du client pour le premier jeton (défaut : paperclip)
  --server-name NOM     nom annoncé à l'agent (défaut : nom de la machine)
  --host NOM            nom DNS utilisé par le client (allowed_hosts), ex. mcp-pve.exemple.fr
  --allow-ip CIDR       réseau autorisé à se connecter (répétable), ex. 203.0.113.10/32
  --pve-cert            Proxmox : copie quotidienne du certificat ACME de pveproxy
  --init TYPE           auto (défaut), systemd, openrc ou none
  --no-restart          mise à jour : ne pas redémarrer le service
  --uninstall           retire binaire, service, sudoers (garde configuration et journaux)
  --purge               avec --uninstall : retire aussi configuration, journaux, utilisateur
  -h, --help            cette aide

--client, --server-name, --host et --allow-ip ne servent qu'à la première installation
(une configuration existante n'est jamais modifiée).
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --dry-run) DRY_RUN=1 ;;
        --uninstall) ACTION=uninstall ;;
        --purge) PURGE=1 ;;
        --pve-cert) PVE_CERT=1 ;;
        --no-restart) RESTART=0 ;;
        --client) CLIENT=${2:?--client attend un nom}; shift ;;
        --server-name) SERVER_NAME=${2:?--server-name attend un nom}; shift ;;
        --host) HOST=${2:?--host attend un nom}; shift ;;
        --allow-ip) ALLOW_IPS="$ALLOW_IPS ${2:?--allow-ip attend un réseau}"; shift ;;
        --init) INIT=${2:?--init attend systemd, openrc, none ou auto}; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "option inconnue : $1" >&2; usage >&2; exit 64 ;;
    esac
    shift
done

# --- Utilitaires --------------------------------------------------------------------------

TODO=
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT INT TERM

say()  { printf '%s\n' "$*"; }
step() { printf '\n==> %s\n' "$*"; }
ok()   { printf '    ok : %s\n' "$*"; }
warn() { printf '    ATTENTION : %s\n' "$*" >&2; }
die()  { printf '\nERREUR : %s\n' "$*" >&2; exit 1; }
todo() { TODO="$TODO
  - $*"; }

# Exécute une commande qui modifie le système, ou l'affiche en simulation.
run() {
    if [ "$DRY_RUN" = 1 ]; then
        printf '    [simulation] %s\n' "$*"
    else
        "$@"
    fi
}

# put SOURCE DESTINATION MODE PROPRIÉTAIRE GROUPE : copie atomique (fichier temporaire + mv).
put() {
    run install -m "$3" -o "$4" -g "$5" "$1" "$2.mcp-shell-tmp"
    run mv -f "$2.mcp-shell-tmp" "$2"
}

same_file() { [ -f "$2" ] && cmp -s "$1" "$2"; }

json_list() { # json_list a b c -> "a", "b", "c"
    out=
    for x in "$@"; do out="$out${out:+, }\"$x\""; done
    printf '%s' "$out"
}

detect_init() {
    [ "$INIT" != auto ] && return
    if [ -d /run/systemd/system ]; then INIT=systemd
    elif command -v rc-update >/dev/null 2>&1; then INIT=openrc
    elif command -v systemctl >/dev/null 2>&1; then INIT=systemd
    else INIT=none
    fi
}

systemd_running() { [ -d /run/systemd/system ]; }

service_active() {
    case "$INIT" in
        systemd) systemd_running && systemctl is-active --quiet mcp-shell ;;
        openrc) rc-service mcp-shell status >/dev/null 2>&1 ;;
        *) return 1 ;;
    esac
}

service_stop() {
    case "$INIT" in
        systemd)
            if systemd_running; then run systemctl disable --now mcp-shell 2>/dev/null || true; fi ;;
        openrc)
            run rc-service mcp-shell stop 2>/dev/null || true
            run rc-update del mcp-shell default 2>/dev/null || true ;;
    esac
}

# Valeur d'une clé simple de config.toml : toml_get cert -> /etc/mcp-shell/fullchain.pem
toml_get() {
    sed -n "s/^[[:space:]]*$1[[:space:]]*=[[:space:]]*\"\([^\"]*\)\".*/\1/p" "$CONFIG" | head -n 1
}

sudo_supports_regex() { # sudo >= 1.9.10
    v=$(sudo -V 2>/dev/null | sed -n '1s/^Sudo version \([0-9][0-9.]*\).*/\1/p')
    [ -n "$v" ] || return 1
    major=$(echo "$v" | cut -d. -f1); minor=$(echo "$v" | cut -d. -f2); patch=$(echo "$v" | cut -d. -f3)
    [ "$major" -gt 1 ] && return 0
    [ "$major" -eq 1 ] && [ "$minor" -gt 9 ] && return 0
    [ "$major" -eq 1 ] && [ "$minor" -eq 9 ] && [ "${patch:-0}" -ge 10 ] && return 0
    return 1
}

# --- Vérifications communes ---------------------------------------------------------------

if [ "$(id -u)" != 0 ]; then
    [ "$DRY_RUN" = 1 ] || die "à lancer en root (sudo ./install.sh)"
    warn "simulation sans root : certaines vérifications peuvent être incomplètes"
fi
detect_init
case "$INIT" in systemd|openrc|none) ;; *) die "--init : systemd, openrc, none ou auto" ;; esac
[ "$DRY_RUN" = 1 ] && say "Mode simulation : rien ne sera modifié."

# --- Désinstallation ----------------------------------------------------------------------

if [ "$ACTION" = uninstall ]; then
    step "Arrêt du service ($INIT)"
    service_stop
    step "Suppression des fichiers installés"
    for f in "$BIN" "$UNIT" "$INITD" "$SUDOERS" "$LOGROTATE" "$PVE_CERT_CRON"; do
        if [ -e "$f" ]; then run rm -f "$f"; ok "$f supprimé"; fi
    done
    [ "$INIT" = systemd ] && systemd_running && run systemctl daemon-reload
    if [ "$PURGE" = 1 ]; then
        step "Purge : configuration, journaux, utilisateur"
        for d in "$ETC" "$LOG_DIR" "$HOME_DIR"; do
            if [ -e "$d" ]; then run rm -rf "$d"; ok "$d supprimé"; fi
        done
        if id "$USER_NAME" >/dev/null 2>&1; then
            if command -v userdel >/dev/null 2>&1; then run userdel "$USER_NAME"
            else run deluser "$USER_NAME"; fi
            grep -q "^$USER_NAME:" /etc/group 2>/dev/null && {
                if command -v groupdel >/dev/null 2>&1; then run groupdel "$USER_NAME"
                else run delgroup "$USER_NAME"; fi
            }
            ok "utilisateur $USER_NAME supprimé"
        fi
    else
        say ""
        say "Conservés : $ETC (configuration, jetons), $LOG_DIR (audit), utilisateur $USER_NAME."
        say "Pour tout retirer : ./install.sh --uninstall --purge"
    fi
    say ""
    say "mcp-shell désinstallé."
    exit 0
fi

# --- Installation / mise à jour -----------------------------------------------------------

step "Vérification de l'archive"
[ -x "$NEW_BIN" ] || die "binaire introuvable : $NEW_BIN (lancer le script depuis l'archive extraite)"
if [ -f "$NEW_BIN.sha256" ]; then
    (cd "$(dirname "$NEW_BIN")" && sha256sum -c mcp-shell.sha256 >/dev/null 2>&1) \
        || die "empreinte SHA-256 du binaire incorrecte : archive corrompue ?"
    ok "empreinte SHA-256 du binaire"
fi
NEW_VERSION=$("$NEW_BIN" --version) || die "le binaire ne s'exécute pas sur cette machine"
ok "$NEW_VERSION"

FIRST_INSTALL=1
[ -f "$CONFIG" ] && FIRST_INSTALL=0
if [ "$FIRST_INSTALL" = 0 ]; then
    OLD_VERSION=$("$BIN" --version 2>/dev/null || echo "aucune")
    say "    mise à jour : $OLD_VERSION -> $NEW_VERSION ($CONFIG existant, non modifié)"
    # La nouvelle version doit accepter la configuration AVANT de toucher à quoi que ce soit.
    "$NEW_BIN" check -c "$CONFIG" >/dev/null 2>"$WORK/check.err" || {
        cat "$WORK/check.err" >&2
        die "$NEW_VERSION refuse la configuration actuelle : rien n'a été modifié (voir CHANGELOG.md)"
    }
    ok "la configuration actuelle est valide pour $NEW_VERSION"
else
    say "    première installation"
fi

WAS_ACTIVE=0
service_active && WAS_ACTIVE=1

step "Utilisateur $USER_NAME"
if id "$USER_NAME" >/dev/null 2>&1; then
    ok "existe déjà"
else
    if command -v useradd >/dev/null 2>&1; then
        run useradd --system --home-dir "$HOME_DIR" --create-home --shell /usr/sbin/nologin "$USER_NAME"
    else
        run addgroup -S "$USER_NAME"
        run adduser -S -D -H -h "$HOME_DIR" -s /sbin/nologin -G "$USER_NAME" "$USER_NAME"
    fi
    ok "créé (système, sans shell de connexion)"
fi
if grep -q '^systemd-journal:' /etc/group 2>/dev/null && command -v usermod >/dev/null 2>&1; then
    if ! id -nG "$USER_NAME" 2>/dev/null | tr ' ' '\n' | grep -qx systemd-journal; then
        run usermod -aG systemd-journal "$USER_NAME"
        ok "ajouté au groupe systemd-journal (journalctl)"
    fi
fi

step "Répertoires"
run install -d -m 0750 -o root -g "$USER_NAME" "$ETC"
run install -d -m 0750 -o "$USER_NAME" -g "$USER_NAME" "$LOG_DIR"
run install -d -m 0750 -o "$USER_NAME" -g "$USER_NAME" "$HOME_DIR"
ok "$ETC, $LOG_DIR, $HOME_DIR"

step "Binaire $BIN"
BIN_CHANGED=0
if same_file "$NEW_BIN" "$BIN"; then
    ok "déjà à jour"
else
    put "$NEW_BIN" "$BIN" 0755 root root
    BIN_CHANGED=1
    ok "installé ($NEW_VERSION)"
fi

step "Configuration $CONFIG"
TOKEN=
if [ "$FIRST_INSTALL" = 0 ]; then
    ok "existante, non modifiée"
else
    TOKEN=$(od -An -tx1 -N32 /dev/urandom | tr -d ' \n')
    [ ${#TOKEN} -eq 64 ] || die "génération du jeton impossible"
    HASH=$(printf '%s' "$TOKEN" | sha256sum | cut -d' ' -f1)
    cp "$PKG/config.example.toml" "$WORK/config.toml"
    sed -i \
        -e "s/^server_name = \"[^\"]*\"/server_name = \"$SERVER_NAME\"/" \
        -e "s/^name = \"paperclip\"/name = \"$CLIENT\"/" \
        -e "s/^sha256 = \"0\{64\}\"/sha256 = \"$HASH\"/" \
        "$WORK/config.toml"
    if [ -n "$HOST" ]; then
        port=$(sed -n 's/^listen = "[^"]*:\([0-9]*\)".*/\1/p' "$WORK/config.toml")
        sed -i "s/^allowed_hosts = .*/allowed_hosts = [$(json_list "$HOST" "$HOST:$port")]/" "$WORK/config.toml"
    fi
    if [ -n "$ALLOW_IPS" ]; then
        # shellcheck disable=SC2086 # découpage voulu
        sed -i "s|^allowed_ips = .*|allowed_ips = [$(json_list $ALLOW_IPS)]|" "$WORK/config.toml"
    fi
    "$NEW_BIN" check -c "$WORK/config.toml" >/dev/null 2>"$WORK/check.err" || {
        cat "$WORK/check.err" >&2
        die "la configuration générée est invalide (option --host ou --allow-ip incorrecte ?)"
    }
    put "$WORK/config.toml" "$CONFIG" 0640 root "$USER_NAME"
    ok "créée à partir de l'exemple, avec un jeton neuf pour le client '$CLIENT'"
fi

step "sudoers $SUDOERS"
if ! command -v sudo >/dev/null 2>&1 || ! command -v visudo >/dev/null 2>&1; then
    warn "sudo n'est pas installé : commandes root impossibles"
    todo "installer sudo (apt install sudo / apk add sudo), puis relancer ./install.sh"
elif ! sudo_supports_regex; then
    warn "sudo $(sudo -V | sed -n '1s/^Sudo version //p') ne gère pas les regex ^…\$ (1.9.10 requis)"
    todo "mettre sudo à jour (>= 1.9.10), puis relancer ./install.sh"
elif ! visudo -cf "$PKG/sudoers.mcp-shell" >/dev/null 2>&1; then
    die "packaging/sudoers.mcp-shell invalide selon visudo : rien n'a été installé pour sudo"
elif [ ! -f "$SUDOERS" ]; then
    put "$PKG/sudoers.mcp-shell" "$SUDOERS" 0440 root root
    ok "installé (validé par visudo)"
elif same_file "$PKG/sudoers.mcp-shell" "$SUDOERS"; then
    ok "identique à celui de l'archive"
else
    ok "existant, NON modifié ; différences avec celui de l'archive :"
    diff -u "$SUDOERS" "$PKG/sudoers.mcp-shell" | sed 's/^/      /' || true
    todo "comparer $SUDOERS avec packaging/sudoers.mcp-shell (diff ci-dessus), puis visudo -cf $SUDOERS"
fi

step "Service ($INIT)"
SVC_CHANGED=0
case "$INIT" in
    systemd)
        if same_file "$PKG/mcp-shell.service" "$UNIT"; then ok "$UNIT déjà à jour"
        else put "$PKG/mcp-shell.service" "$UNIT" 0644 root root; SVC_CHANGED=1; ok "$UNIT installé"; fi
        if systemd_running; then
            run systemctl daemon-reload
            run systemctl enable mcp-shell >/dev/null 2>&1
            ok "activé au démarrage"
        else
            warn "systemd ne tourne pas : unité installée mais non activée"
        fi
        say "    personnalisation : systemctl edit mcp-shell (le fichier installé est remplacé à chaque mise à jour)"
        ;;
    openrc)
        if same_file "$PKG/mcp-shell.openrc" "$INITD"; then ok "$INITD déjà à jour"
        else put "$PKG/mcp-shell.openrc" "$INITD" 0755 root root; SVC_CHANGED=1; ok "$INITD installé"; fi
        run rc-update add mcp-shell default >/dev/null 2>&1 || true
        ok "activé au démarrage (runlevel default)"
        ;;
    none)
        warn "aucun gestionnaire de services détecté : à lancer soi-même"
        todo "lancer le service : su -s /bin/sh $USER_NAME -c '$BIN serve -c $CONFIG'"
        ;;
esac

step "Rotation des journaux"
if [ -d /etc/logrotate.d ]; then
    if same_file "$PKG/mcp-shell.logrotate" "$LOGROTATE"; then ok "déjà à jour"
    else put "$PKG/mcp-shell.logrotate" "$LOGROTATE" 0644 root root; ok "$LOGROTATE installé"; fi
else
    ok "logrotate absent : rien à faire (penser à la rotation de $LOG_DIR/audit.jsonl)"
fi

if [ "$PVE_CERT" = 1 ]; then
    step "Certificat Proxmox"
    [ -f /etc/pve/local/pveproxy-ssl.pem ] \
        || die "/etc/pve/local/pveproxy-ssl.pem absent : configurer d'abord ACME (Datacenter -> ACME)"
    cat > "$WORK/cert" <<EOF
#!/bin/sh
# Installé par mcp-shell/install.sh : copie le certificat ACME de Proxmox pour mcp-shell.
# mcp-shell le recharge à chaud (reload_interval_secs).
install -m 0640 -g $USER_NAME /etc/pve/local/pveproxy-ssl.pem $ETC/fullchain.pem
install -m 0640 -g $USER_NAME /etc/pve/local/pveproxy-ssl.key $ETC/privkey.pem
EOF
    put "$WORK/cert" "$PVE_CERT_CRON" 0755 root root
    run "$PVE_CERT_CRON"
    ok "certificat copié ; copie quotidienne via $PVE_CERT_CRON"
fi

# --- Démarrage / redémarrage --------------------------------------------------------------

if [ "$FIRST_INSTALL" = 0 ]; then
    if [ "$WAS_ACTIVE" = 1 ] && [ "$BIN_CHANGED" = 0 ] && [ "$SVC_CHANGED" = 0 ]; then
        say ""
        say "    binaire et service inchangés : pas de redémarrage"
    elif [ "$WAS_ACTIVE" = 1 ] && [ "$RESTART" = 1 ]; then
        step "Redémarrage du service"
        case "$INIT" in
            systemd) run systemctl restart mcp-shell ;;
            openrc) run rc-service mcp-shell restart ;;
        esac
        ok "redémarré"
    elif [ "$WAS_ACTIVE" = 1 ]; then
        todo "redémarrer le service pour utiliser $NEW_VERSION"
    fi
fi

# --- Ce qui reste à faire -----------------------------------------------------------------

if [ -f "$CONFIG" ] && [ "$DRY_RUN" = 0 ]; then
    CERT=$(toml_get cert)
    SHELL_PATH=$(toml_get shell)
    if grep -q '^\[tls\]' "$CONFIG"; then
        if [ ! -f "$CERT" ]; then
            todo "certificat TLS absent ($CERT) : Let's Encrypt au nom du client, ou --pve-cert sur Proxmox, ou reverse proxy local (README §3)"
        elif ! su -s /bin/sh "$USER_NAME" -c "test -r '$CERT'" 2>/dev/null; then
            todo "$CERT illisible par l'utilisateur $USER_NAME (groupe $USER_NAME, mode 0640)"
        fi
    fi
    grep -q 'exemple\.fr' "$CONFIG" && todo "allowed_hosts : remplacer mcp-pve.exemple.fr par le nom réel (ou relancer avec --host sur une nouvelle installation)"
    grep -q '"203\.0\.113\.10/32"' "$CONFIG" && todo "allowed_ips : remplacer 203.0.113.10/32 (adresse d'exemple) par l'IP du client"
    [ -n "$SHELL_PATH" ] && [ ! -x "$SHELL_PATH" ] && todo "shell $SHELL_PATH absent (apt install bash / apk add bash) ou changer [exec].shell"
fi
if [ "$FIRST_INSTALL" = 1 ]; then
    todo "relire [exec].allow et deny dans $CONFIG, et $SUDOERS en cohérence (README §5)"
    todo "tester : $BIN check -c $CONFIG --command 'uptime'"
    case "$INIT" in
        systemd) todo "démarrer : systemctl start mcp-shell && journalctl -u mcp-shell -n 20" ;;
        openrc) todo "démarrer : rc-service mcp-shell start && tail $LOG_DIR/service.log" ;;
    esac
fi

say ""
say "==========================================================================="
if [ "$DRY_RUN" = 1 ]; then
    say "Simulation terminée : rien n'a été modifié."
elif [ "$FIRST_INSTALL" = 1 ]; then
    say "$NEW_VERSION installé. Le service n'est PAS démarré."
else
    say "$NEW_VERSION installé (mise à jour)."
fi

if [ -n "$TOKEN" ] && [ "$DRY_RUN" = 0 ]; then
    url_host=${HOST:-<nom-du-serveur>}
    port=$(sed -n 's/^listen = "[^"]*:\([0-9]*\)".*/\1/p' "$CONFIG")
    say ""
    say "Jeton du client '$CLIENT' (affiché une seule fois, seule son empreinte est stockée) :"
    say ""
    say "  $TOKEN"
    say ""
    say "Déclaration côté client (Claude Code) :"
    say "  claude mcp add --transport http --scope project $SERVER_NAME \\"
    say "    https://$url_host:$port/mcp \\"
    say "    --header \"Authorization: Bearer $TOKEN\""
fi

if [ -n "$TODO" ]; then
    say ""
    say "Reste à faire :$TODO"
fi
say "==========================================================================="
