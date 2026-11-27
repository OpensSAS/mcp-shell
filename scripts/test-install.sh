#!/bin/sh
# INIT_OPT est volontairement découpé en mots (vide ou "--init systemd").
# shellcheck disable=SC2086
# Test de bout en bout de install.sh dans un conteneur jetable.
#
#   scripts/test-install.sh IMAGE ARCHIVE
#   ex. scripts/test-install.sh alpine:3.20 dist/mcp-shell-0.1.3-x86_64-linux.tar.gz
#
# Dans le conteneur : simulation, première installation, démarrage du serveur avec un
# certificat auto-signé, appels MCP réels (dont une commande via sudo et les regex sudoers),
# mise à jour idempotente, sudoers modifié non écrasé, désinstallation, purge.
set -eu

if [ "${1:-}" != --inside ]; then
    IMAGE=${1:?image Docker attendue (debian:12, alpine:3.20…)}
    ARCHIVE=${2:?archive de release attendue}
    ARCHIVE=$(cd "$(dirname "$ARCHIVE")" && pwd)/$(basename "$ARCHIVE")
    SELF=$(cd "$(dirname "$0")" && pwd)/$(basename "$0")
    exec docker run --rm \
        -v "$ARCHIVE:/tmp/mcp-shell.tar.gz:ro" \
        -v "$SELF:/tmp/test-install.sh:ro" \
        "$IMAGE" sh /tmp/test-install.sh --inside
fi

# ------------------------------------------------------------------ dans le conteneur ------

fail() { printf '\nÉCHEC : %s\n' "$*" >&2; exit 1; }
pass() { printf '  ✓ %s\n' "$*"; }
title() { printf '\n### %s\n' "$*"; }

title "Dépendances"
if command -v apk >/dev/null 2>&1; then
    apk add -q bash sudo curl openssl openrc logrotate >/dev/null || fail "apk add"
    INIT_OPT=          # détection automatique : OpenRC
    EXPECT_SERVICE=/etc/init.d/mcp-shell
else
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq >/dev/null || fail "apt-get update"
    apt-get install -y -qq sudo curl openssl logrotate procps >/dev/null || fail "apt-get install"
    INIT_OPT="--init systemd"   # pas de systemd dans le conteneur : unité installée, non activée
    EXPECT_SERVICE=/etc/systemd/system/mcp-shell.service
fi
grep -E '^(PRETTY_NAME)=' /etc/os-release
sudo -V | head -n 1

# Faux outils Proxmox pour tester sudo de bout en bout.
for b in pct qm; do
    printf '#!/bin/sh\necho "%s-factice $*"\n' "$b" > /usr/sbin/$b
    chmod 0755 /usr/sbin/$b
done

mkdir -p /tmp/a && tar xzf /tmp/mcp-shell.tar.gz -C /tmp/a
cd /tmp/a/mcp-shell-*
[ -x install.sh ] || fail "install.sh absent de l'archive"

title "Simulation"
./install.sh --dry-run $INIT_OPT > /tmp/dry.log 2>&1 || { cat /tmp/dry.log; fail "simulation"; }
[ ! -e /etc/mcp-shell ] || fail "la simulation a créé /etc/mcp-shell"
[ ! -e /usr/local/bin/mcp-shell ] || fail "la simulation a installé le binaire"
id mcp-shell >/dev/null 2>&1 && fail "la simulation a créé l'utilisateur"
grep -q 'rien ne sera modifié' /tmp/dry.log || fail "message de simulation absent"
pass "rien n'est modifié"

title "Première installation"
./install.sh $INIT_OPT --client ci --server-name ci-test --host mcp-ci.exemple.org \
    --allow-ip 127.0.0.1/32 --allow-ip 203.0.113.0/24 > /tmp/install.log 2>&1 \
    || { cat /tmp/install.log; fail "installation"; }
cat /tmp/install.log
TOKEN=$(sed -n 's/^  \([0-9a-f]\{64\}\)$/\1/p' /tmp/install.log)
[ ${#TOKEN} -eq 64 ] || fail "jeton non affiché"
pass "jeton affiché"

perm() { stat -c '%a %U:%G' "$1"; }
[ "$(perm /usr/local/bin/mcp-shell)" = "755 root:root" ] || fail "binaire : $(perm /usr/local/bin/mcp-shell)"
[ "$(perm /etc/mcp-shell)" = "750 root:mcp-shell" ] || fail "/etc/mcp-shell : $(perm /etc/mcp-shell)"
[ "$(perm /etc/mcp-shell/config.toml)" = "640 root:mcp-shell" ] || fail "config : $(perm /etc/mcp-shell/config.toml)"
[ "$(perm /etc/sudoers.d/mcp-shell)" = "440 root:root" ] || fail "sudoers : $(perm /etc/sudoers.d/mcp-shell)"
[ "$(perm /var/log/mcp-shell)" = "750 mcp-shell:mcp-shell" ] || fail "journaux : $(perm /var/log/mcp-shell)"
[ -f "$EXPECT_SERVICE" ] || fail "service absent : $EXPECT_SERVICE"
[ -f /etc/logrotate.d/mcp-shell ] || fail "logrotate absent"
logrotate -d /etc/logrotate.d/mcp-shell >/dev/null 2>&1 || fail "logrotate refuse la configuration"
pass "fichiers et droits"

C=/etc/mcp-shell/config.toml
HASH=$(printf '%s' "$TOKEN" | sha256sum | cut -d' ' -f1)
grep -q "^sha256 = \"$HASH\"" $C || fail "empreinte du jeton absente de la configuration"
grep -q '^name = "ci"' $C || fail "nom du client"
grep -q '^server_name = "ci-test"' $C || fail "server_name"
grep -q '^allowed_hosts = \["mcp-ci.exemple.org", "mcp-ci.exemple.org:8443"\]' $C || fail "allowed_hosts"
grep -q '^allowed_ips = \["127.0.0.1/32", "203.0.113.0/24"\]' $C || fail "allowed_ips"
/usr/local/bin/mcp-shell check -c $C > /dev/null || fail "mcp-shell check"
grep -q 'certificat TLS absent' /tmp/install.log || fail "le certificat manquant n'est pas signalé"
grep -q "allowed_ips : remplacer" /tmp/install.log && fail "faux rappel allowed_ips (valeurs fournies par --allow-ip)"
pass "configuration générée et valide"

title "Démarrage avec un certificat auto-signé"
openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj "/CN=mcp-ci.exemple.org" \
    -addext "subjectAltName=DNS:mcp-ci.exemple.org" \
    -keyout /etc/mcp-shell/privkey.pem -out /etc/mcp-shell/fullchain.pem 2>/dev/null
chgrp mcp-shell /etc/mcp-shell/*.pem && chmod 0640 /etc/mcp-shell/*.pem
su -s /bin/sh mcp-shell -c '/usr/local/bin/mcp-shell serve -c /etc/mcp-shell/config.toml' \
    > /tmp/serve.log 2>&1 &
SERVER_PID=$!
URL=https://mcp-ci.exemple.org:8443
CURL="curl -sS --cacert /etc/mcp-shell/fullchain.pem --resolve mcp-ci.exemple.org:8443:127.0.0.1"
i=0
until $CURL $URL/health >/dev/null 2>&1; do
    i=$((i + 1)); [ $i -lt 50 ] || { cat /tmp/serve.log; fail "le serveur ne répond pas"; }
    sleep 0.2
done
[ "$($CURL $URL/health)" = ok ] || fail "/health"
pass "HTTPS, /health"

H_JSON='Content-Type: application/json'
H_ACC='Accept: application/json, text/event-stream'
code=$($CURL -o /dev/null -w '%{http_code}' -H "Authorization: Bearer mauvais" -H "$H_JSON" -H "$H_ACC" \
    $URL/mcp -d '{}')
[ "$code" = 401 ] || fail "mauvais jeton : HTTP $code au lieu de 401"
pass "mauvais jeton refusé (401)"

AUTH="Authorization: Bearer $TOKEN"
SID=$($CURL -D - -o /dev/null -H "$AUTH" -H "$H_JSON" -H "$H_ACC" $URL/mcp \
    -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"ci","version":"1"}}}' \
    | awk -F': ' 'tolower($1)=="mcp-session-id"{print $2}' | tr -d '\r')
[ -n "$SID" ] || fail "initialisation MCP"
$CURL -o /dev/null -H "$AUTH" -H "$H_JSON" -H "$H_ACC" -H "mcp-session-id: $SID" $URL/mcp \
    -d '{"jsonrpc":"2.0","method":"notifications/initialized"}'
call() {
    $CURL -H "$AUTH" -H "$H_JSON" -H "$H_ACC" -H "mcp-session-id: $SID" $URL/mcp \
        -d "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"run_command\",\"arguments\":{\"command\":\"$1\"}}}"
}
call 'sudo -n /usr/sbin/pct config 100' | grep -q 'pct-factice config 100' \
    || { call 'sudo -n /usr/sbin/pct config 100'; fail "commande via sudo"; }
pass "run_command via sudo (regex sudoers acceptée)"
call 'sudo -n /usr/sbin/pct config 100 --current' | grep -q '"denied":true' || fail "commande hors liste blanche"
pass "commande hors liste blanche refusée"
su -s /bin/sh mcp-shell -c 'sudo -n /usr/sbin/pct config 100 --current' >/dev/null 2>&1 \
    && fail "sudoers accepte des arguments supplémentaires"
pass "sudoers refuse les arguments supplémentaires"
grep -q '"outcome":"executed"' /var/log/mcp-shell/audit.jsonl || fail "journal d'audit"
pass "journal d'audit"
kill $SERVER_PID 2>/dev/null || true
wait $SERVER_PID 2>/dev/null || true

title "Relance (mise à jour) : rien n'est écrasé"
SUM_BEFORE=$(sha256sum $C)
echo "# modification locale" >> /etc/sudoers.d/mcp-shell
./install.sh $INIT_OPT > /tmp/upgrade.log 2>&1 || { cat /tmp/upgrade.log; fail "relance"; }
cat /tmp/upgrade.log
[ "$(sha256sum $C)" = "$SUM_BEFORE" ] || fail "la configuration a été modifiée"
grep -q 'modification locale' /etc/sudoers.d/mcp-shell || fail "le sudoers local a été écrasé"
grep -q 'NON modifié' /tmp/upgrade.log || fail "différence sudoers non signalée"
grep -Eq '^  [0-9a-f]{64}$' /tmp/upgrade.log && fail "un nouveau jeton a été affiché"
grep -q 'déjà à jour' /tmp/upgrade.log || fail "binaire signalé comme modifié"
pass "configuration et sudoers conservés, pas de nouveau jeton"

title "Configuration refusée par la nouvelle version : rien n'est touché"
cp $C /tmp/config.bak
echo 'option_inconnue = 1' >> $C
cp /usr/local/bin/mcp-shell /tmp/bin.bak
if ./install.sh $INIT_OPT > /tmp/bad.log 2>&1; then cat /tmp/bad.log; fail "config invalide acceptée"; fi
grep -q "rien n'a été modifié" /tmp/bad.log || { cat /tmp/bad.log; fail "message d'arrêt"; }
cmp -s /usr/local/bin/mcp-shell /tmp/bin.bak || fail "binaire modifié malgré l'erreur"
cp /tmp/config.bak $C
pass "arrêt avant toute modification"

title "Désinstallation"
./install.sh --uninstall $INIT_OPT > /tmp/un.log 2>&1 || { cat /tmp/un.log; fail "désinstallation"; }
[ ! -e /usr/local/bin/mcp-shell ] || fail "binaire encore présent"
[ ! -e /etc/sudoers.d/mcp-shell ] || fail "sudoers encore présent"
[ ! -e "$EXPECT_SERVICE" ] || fail "service encore présent"
[ -f $C ] || fail "la configuration ne devait pas être supprimée"
id mcp-shell >/dev/null 2>&1 || fail "l'utilisateur ne devait pas être supprimé"
pass "binaire, service, sudoers retirés ; configuration et utilisateur conservés"

./install.sh --uninstall --purge $INIT_OPT > /tmp/purge.log 2>&1 || { cat /tmp/purge.log; fail "purge"; }
[ ! -e /etc/mcp-shell ] || fail "/etc/mcp-shell encore présent"
[ ! -e /var/log/mcp-shell ] || fail "/var/log/mcp-shell encore présent"
id mcp-shell >/dev/null 2>&1 && fail "utilisateur encore présent"
grep -q '^mcp-shell:' /etc/group && fail "groupe encore présent"
pass "purge complète"

printf '\n%s\n' "TOUS LES TESTS D'INSTALLATION ONT RÉUSSI"
