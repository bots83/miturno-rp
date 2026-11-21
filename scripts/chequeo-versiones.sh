#!/usr/bin/env bash
# Regla de versiones: todo paquete de Cargo.lock (también los transitivos) tiene que haberse publicado
# hace al menos 14 días. Consulta a crates.io la fecha de cada versión exacta (se guarda en caché porque
# no cambia) y lista "nombre versión fecha" de los que son más nuevos. Las dependencias de ruta o git se ignoran.
# Uso: bash scripts/chequeo-versiones.sh [Cargo.lock]
# Variables: CHEQUEO_FETCH (comando "nombre versión" que imprime created_at; para tests), CHEQUEO_CACHE, DIAS (14).
set -euo pipefail
LOCK=${1:-Cargo.lock}
DIAS=${DIAS:-14}
CACHE=${CHEQUEO_CACHE:-${XDG_CACHE_HOME:-$HOME/.cache}/miturno-rp/versiones}
mkdir -p "$(dirname "$CACHE")"; touch "$CACHE"
CORTE=$(date -u -d "$DIAS days ago" +%Y-%m-%dT%H:%M:%S)

consultar() {
  if [ -n "${CHEQUEO_FETCH:-}" ]; then "$CHEQUEO_FETCH" "$1" "$2"; return; fi
  curl -fsS --retry 3 -A miturno-build-check "https://crates.io/api/v1/crates/$1/$2" | jq -r '.version.created_at'
}

[ -r "$LOCK" ] || { echo "chequeo-versiones: no puedo leer $LOCK" >&2; exit 2; }
mapfile -t PAQUETES < <(awk '
  /^\[\[package\]\]/ { n=""; v="" }
  /^name = /    { gsub(/"/,"",$3); n=$3 }
  /^version = / && n != "" { gsub(/"/,"",$3); v=$3 }
  /^source = "registry\+/ { print n " " v }' "$LOCK")
# Falla cerrado: 0 paquetes quiere decir un lock vacío, roto o de un formato que no se entiende.
if [ "${#PAQUETES[@]}" = 0 ]; then
  echo "chequeo-versiones: $LOCK no tiene ningún paquete de crates.io; no sigo (¿lock roto o de otro formato?)" >&2; exit 2
fi

nuevos=0
for p in "${PAQUETES[@]}"; do
  n=${p% *}; v=${p#* }
  fecha=$(awk -v k="$n $v" '$1" "$2==k {print $3; exit}' "$CACHE")
  if [ -z "$fecha" ]; then
    fecha=$(consultar "$n" "$v" 2>/dev/null || true)
    if ! [[ "$fecha" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}T ]]; then
      echo "chequeo-versiones: no pude obtener la fecha de $n $v" >&2; exit 2
    fi
    echo "$n $v $fecha" >> "$CACHE"
  fi
  if [[ "${fecha:0:19}" > "$CORTE" ]]; then echo "$n $v $fecha"; nuevos=$((nuevos + 1)); fi
done
if [ "$nuevos" -gt 0 ]; then
  echo "chequeo-versiones: $nuevos paquetes publicados hace menos de $DIAS días (corte $CORTE)." >&2; exit 1
fi
echo "chequeo-versiones: ${#PAQUETES[@]} paquetes, todos de hace $DIAS días o más."
