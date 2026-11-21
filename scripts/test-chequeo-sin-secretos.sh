#!/usr/bin/env bash
# Tests de scripts/chequeo-sin-secretos.sh en repos temporales. Uso: bash scripts/test-chequeo-sin-secretos.sh
set -euo pipefail
AQUI=$(cd "$(dirname "$0")" && pwd)
CHEQUEO="$AQUI/chequeo-sin-secretos.sh"
T=$(mktemp -d); trap 'rm -rf "$T"' EXIT
fallos=0
export CASA_PATRON="mt-prueba.ejemplo" CASA_IP="203.0.113.7"

nuevo_repo() { rm -rf "$T/r"; mkdir -p "$T/r"; git -C "$T/r" init -q -b main; git -C "$T/r" config user.email t@t; git -C "$T/r" config user.name t; echo "hola" > "$T/r/README.md"; git -C "$T/r" add -A; git -C "$T/r" commit -qm base; }
correr() { (cd "$T/r" && bash "$CHEQUEO" "$@") > "$T/salida" 2>&1; }
esperar() { if eval "$2"; then echo "ok   $1"; else echo "FALLA $1"; sed 's/^/     | /' "$T/salida"; fallos=$((fallos + 1)); fi; }

nuevo_repo
correr --todo && r=0 || r=$?
esperar "repo limpio: pasa" '[ "$r" = 0 ]'

# Cada caso: nombre, contenido plantado (el valor que no debe aparecer en la salida).
casos=(
  "nombre DDNS de casa|PS5_HOST=mt-prueba.ejemplo"
  "IP de casa|conecto a 203.0.113.7:9295"
  "clave de Kick|sk_us-west-2_AbCdEfGh1234567890"
  "clave privada PEM|-----BEGIN PRIVATE KEY-----"
  "clave privada PEM con tipo|-----BEGIN OPENSSH PRIVATE KEY-----"
  "variable de Kick con valor|KICK_STREAM_KEY=loquesea"
  "variable de Telegram con valor|TELEGRAM_BOT_TOKEN=loquesea"
  "registración de la PS5|rp_regist_key=@ByteArray(abc)"
  "token de PSN|psn_refresh_token=abc"
  "token de bot de Telegram|1234567890:AAEhBP0av18SuperSecretoDeBotDeTelegramX"
  "registración de la PS5 en ps5.env|PS5_REGIST_KEY=9f8e7d6c"
  "clave de despertar de la PS5|PS5_MORNING=00112233445566778899aabbccddeeff"
  "cuenta PSN de la PS5|PS5_ACCOUNT_ID=\"QUJDREVGR0g=\""
  "variable VIDEO_ con valor|VIDEO_CTL_TOKEN=abc123def456"
  "secreto de coturn|static-auth-secret=0123456789abcdef"
)
for caso in "${casos[@]}"; do
  nombre=${caso%%|*}; valor=${caso#*|}
  nuevo_repo; printf '%s\n' "$valor" > "$T/r/config.txt"
  correr --todo && r=0 || r=$?
  esperar "detecta: $nombre" '[ "$r" != 0 ]'
  esperar "no imprime el valor: $nombre" '! grep -qF -- "$valor" "$T/salida"'
  esperar "dice en qué archivo: $nombre" 'grep -q "config.txt" "$T/salida"'
done

nuevo_repo
echo "KICK_STREAM_KEY=" > "$T/r/ejemplo.txt"; echo "rtmps://ingest.ejemplo/app/" >> "$T/r/ejemplo.txt"
correr --todo && r=0 || r=$?
esperar "variable vacía de ejemplo (documentación): pasa" '[ "$r" = 0 ]'

nuevo_repo
{ echo "# (static-auth-secret=… lo agrega desplegar-video.sh)"; echo "VIDEO_CTL_URL="; echo 'PS5_REGIST_KEY=$CLAVE'; } > "$T/r/ejemplo.txt"
correr --todo && r=0 || r=$?
esperar "PS5_/VIDEO_/static-auth-secret sin valor o con un marcador: pasa" '[ "$r" = 0 ]'

nuevo_repo
mkdir -p "$T/r/crates/miturno-rp/src"
printf '    const PS5: &str = "PS5_REGIST_KEY=a1b2c3d4\\nPS5_MORNING=000102030405060708090a0b0c0d0e0f\\nPS5_ACCOUNT_ID=\\"NDJvGJAjYVQ=\\"\\n";\n' > "$T/r/crates/miturno-rp/src/config.rs"
git -C "$T/r" add -A; git -C "$T/r" commit -qm "fixture"
correr --historial && r=0 || r=$?
esperar "el ps5.env falso de los tests de config.rs no cuenta (tampoco en el historial)" '[ "$r" = 0 ]'
printf '    const PS5: &str = "PS5_REGIST_KEY=a1b2c3d5\\n";\n' > "$T/r/crates/miturno-rp/src/config.rs"
correr --todo && r=0 || r=$?
esperar "otro valor en el mismo lugar sí cuenta" '[ "$r" != 0 ]'

nuevo_repo
echo "sk_us-west-2_AbCdEfGh1234567890" > "$T/r/viejo.txt"; git -C "$T/r" add -A; git -C "$T/r" commit -qm "oops"
git -C "$T/r" rm -q viejo.txt; git -C "$T/r" commit -qm "borrado"
correr --todo && r=0 || r=$?
esperar "árbol actual limpio aunque el historial tenga algo: --todo pasa" '[ "$r" = 0 ]'
correr --historial && r=0 || r=$?
esperar "--historial detecta el secreto borrado en un commit anterior" '[ "$r" != 0 ]'
esperar "--historial no imprime el valor" '! grep -qF "AbCdEfGh1234567890" "$T/salida"'

nuevo_repo
echo "mt-prueba.ejemplo" > "$T/r/nuevo.txt"
correr --todo && r=0 || r=$?
esperar "detecta archivos nuevos todavía sin commit" '[ "$r" != 0 ]'

nuevo_repo
(cd "$T/r" && env -u CASA_PATRON -u CASA_IP CASA_SSH=/bin/false bash "$CHEQUEO" --todo) > "$T/salida" 2>&1 && r=0 || r=$?
esperar "sin poder leer el nombre de casa, falla cerrado" '[ "$r" != 0 ] && grep -qi "casa" "$T/salida"'

[ "$fallos" = 0 ] && echo "todos los tests pasan" || { echo "$fallos test(s) fallan"; exit 1; }
