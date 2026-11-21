#!/usr/bin/env bash
# Tests de scripts/chequeo-versiones.sh sin red: la consulta a crates.io se reemplaza con CHEQUEO_FETCH.
# Uso: bash scripts/test-chequeo-versiones.sh
set -euo pipefail
AQUI=$(cd "$(dirname "$0")" && pwd)
CHEQUEO="$AQUI/chequeo-versiones.sh"
T=$(mktemp -d); trap 'rm -rf "$T"' EXIT
fallos=0
esperar() { if eval "$2"; then echo "ok   $1"; else echo "FALLA $1"; sed 's/^/     | /' "$T/salida"; fallos=$((fallos + 1)); fi; }

# Falso crates.io: el nombre decide la fecha. "nuevo*" es de hoy, el resto es viejo.
cat > "$T/fetch.sh" <<'F'
#!/usr/bin/env bash
case "$1" in nuevo*) date -u +%Y-%m-%dT%H:%M:%S.000000+00:00 ;; *) echo "2020-01-01T00:00:00.000000+00:00" ;; esac
F
chmod +x "$T/fetch.sh"
export CHEQUEO_FETCH="$T/fetch.sh"
correr() { CHEQUEO_CACHE="$T/cache-$1" bash "$CHEQUEO" "$T/$2.lock" > "$T/salida" 2>&1; }
reg='source = "registry+https://github.com/rust-lang/crates.io-index"'

printf 'version = 4\n\n[[package]]\nname = "viejo"\nversion = "1.0.0"\n%s\n\n[[package]]\nname = "rp-core"\nversion = "0.1.0"\n' "$reg" > "$T/limpio.lock"
correr a limpio && r=0 || r=$?
esperar "todo viejo: pasa" '[ "$r" = 0 ]'

printf 'version = 4\n\n[[package]]\nname = "viejo"\nversion = "1.0.0"\n%s\n\n[[package]]\nname = "nuevo-crate"\nversion = "2.3.4"\n%s\n' "$reg" "$reg" > "$T/malo.lock"
correr b malo && r=0 || r=$?
esperar "uno demasiado nuevo: falla" '[ "$r" != 0 ]'
esperar "nombra el crate y la versión" 'grep -q "nuevo-crate 2.3.4" "$T/salida"'
esperar "no nombra al viejo" '! grep -q "viejo 1.0.0" "$T/salida"'

printf 'version = 4\n\n[[package]]\nname = "nuevo-git"\nversion = "1.0.0"\nsource = "git+https://example.com/x#abc"\n\n[[package]]\nname = "nuevo-ruta"\nversion = "1.0.0"\n' > "$T/git.lock"
correr c git && r=0 || r=$?
esperar "git y ruta se ignoran: pasa" '[ "$r" = 0 ]'

# La respuesta queda en el caché: con el fetch roto, sigue andando.
CHEQUEO_FETCH=/bin/false correr a limpio && r=0 || r=$?
esperar "usa el caché" '[ "$r" = 0 ]'

# Si la consulta falla y no hay caché, falla cerrado.
CHEQUEO_FETCH=/bin/false correr d limpio && r=0 || r=$?
esperar "sin respuesta de crates.io: falla" '[ "$r" != 0 ]'

[ "$fallos" = 0 ] && echo "todo bien" || { echo "$fallos fallos"; exit 1; }
