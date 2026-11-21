#!/usr/bin/env bash
# Chequeo de secretos de miturno-rp (repo público): falla si encuentra secretos o datos de la red de casa.
#   --todo       árbol de trabajo: archivos versionados y nuevos no ignorados
#   --historial  todos los commits (lo que se publica con un push, aunque después se haya borrado)
# Nunca imprime lo que encontró: solo dónde y qué regla.
# El nombre DDNS y la IP de casa NO están escritos acá: se leen del servidor de video en cada corrida
# (o de CASA_PATRON / CASA_IP). Si no se pueden leer, falla cerrado.
set -euo pipefail
MODO=${1:---todo}
CASA_SSH=${CASA_SSH:-ssh -o BatchMode=yes -o ConnectTimeout=10 miturno-video}

if [ -z "${CASA_PATRON:-}" ]; then
  CASA_PATRON=$($CASA_SSH "sudo sed -n 's/^PS5_HOST=//p' /etc/miturno-video/casa.env" 2>/dev/null || true)
fi
if [ -z "${CASA_IP:-}" ] && [ -n "${CASA_PATRON:-}" ]; then
  CASA_IP=$(getent ahostsv4 "$CASA_PATRON" 2>/dev/null | awk 'NR==1{print $1}' || true)
fi
if [ -z "${CASA_PATRON:-}" ] || [ -z "${CASA_IP:-}" ]; then
  echo "chequeo-sin-secretos: no pude leer el nombre o la IP de casa (servidor de video o CASA_PATRON/CASA_IP). No sigo: sin eso el chequeo no es confiable." >&2
  exit 2
fi

# Regla|tipo (F = texto fijo, E = expresión regular)|patrón
# Los valores de PS5_*, VIDEO_* y static-auth-secret cuentan si empiezan como un valor de verdad (letra,
# número o base64, con o sin comillas): "=", "=$VAR", "=…" o "=<…>" son documentación.
VALOR="[\"']?[A-Za-z0-9+/]"
# El ps5.env falso de los tests de crates/miturno-rp/src/config.rs (tal cual, también en el historial).
PS5_FALSO='PS5_REGIST_KEY=a1b2c3d4\\nPS5_MORNING=000102030405060708090a0b0c0d0e0f\\nPS5_ACCOUNT_ID=\\"NDJvGJAjYVQ=\\"'
REGLAS=(
  "nombre DDNS de casa|F|$CASA_PATRON"
  "IP de casa|F|$CASA_IP"
  "clave de transmisión de Kick|E|sk_[a-z]{2}-[a-z]+-[0-9]+_[A-Za-z0-9]{8,}"
  "clave privada|E|BEGIN [A-Z ]*PRIVATE KEY"
  "KICK_STREAM_KEY con valor|E|KICK_STREAM_KEY=[^[:space:]\"'\`]"
  "TELEGRAM_BOT_TOKEN con valor|E|TELEGRAM_BOT_TOKEN=[^[:space:]\"'\`]"
  "registración de la PS5|E|rp_regist_key="
  "token de PSN|E|psn_(refresh|auth)_token="
  "token de bot de Telegram|E|[0-9]{8,}:[A-Za-z0-9_-]{30,}"
  "datos de la PS5 (ps5.env) con valor|E|PS5_(REGIST_KEY|MORNING|ACCOUNT_ID)=$VALOR"
  "variable VIDEO_ con valor|E|VIDEO_[A-Z_]+=$VALOR"
  "secreto de coturn (static-auth-secret) con valor|E|static-auth-secret=$VALOR"
)
# Excepciones por regla (expresión regular): las líneas que la tienen no cuentan para esa regla.
declare -A EXCEPTO=(["datos de la PS5 (ps5.env) con valor"]="$PS5_FALSO")
# Este script y su test contienen los patrones a propósito.
EXCLUIR=(":(exclude)scripts/chequeo-sin-secretos.sh" ":(exclude)scripts/test-chequeo-sin-secretos.sh")

case "$MODO" in
  --todo) DONDE=(--untracked); REVS=() ;;
  --historial) DONDE=(); mapfile -t REVS < <(git rev-list --all) ;;
  *) echo "uso: chequeo-sin-secretos.sh --todo|--historial" >&2; exit 2 ;;
esac
[ "$MODO" = --historial ] && [ "${#REVS[@]}" = 0 ] && { echo "sin commits: nada que revisar"; exit 0; }

hallazgos=0
for regla in "${REGLAS[@]}"; do
  nombre=${regla%%|*}; resto=${regla#*|}; tipo=${resto%%|*}; patron=${resto#*|}
  excepto=(); [ -n "${EXCEPTO[$nombre]:-}" ] && excepto=(--and --not -e "${EXCEPTO[$nombre]}")
  bandera=-E; [ "$tipo" = F ] && bandera=-F
  # -c: solo "archivo:cantidad" (o "commit:archivo:cantidad"); el contenido nunca sale.
  if salida=$(git grep -c -I "$bandera" -e "$patron" "${excepto[@]}" "${DONDE[@]}" "${REVS[@]}" -- . "${EXCLUIR[@]}" 2>/dev/null); then
    while IFS= read -r linea; do
      donde=${linea%:*}
      [ "$MODO" = --historial ] && donde="commit ${donde:0:12}"
      echo "SECRETO ($nombre): $donde"
      hallazgos=$((hallazgos + 1))
    done <<<"$salida"
  fi
done

if [ "$hallazgos" -gt 0 ]; then
  echo "chequeo-sin-secretos: $hallazgos hallazgo(s). No publiques esto." >&2
  exit 1
fi
echo "chequeo-sin-secretos ($MODO): sin secretos ni datos de casa."
