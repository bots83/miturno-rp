#!/usr/bin/env bash
# Tests de config/kick-destino.sh: cómo se arma el destino RTMPS de kick-out a partir de kick.env.
# Kick usa Amazon IVS: la publicación tiene que ir a rtmps://<ingesta>:443/app/<clave>. Si a la URL
# cargada le falta /app (Kick la muestra a veces sin ruta), IVS acepta TLS y el saludo RTMP pero corta
# al publicar ("Error opening output … Input/output error").
set -u
cd "$(dirname "$0")/.."
fallas=0
caso() { # nombre, esperado, URL, clave[, destino fijo]
  local r s
  r=$(env -i KICK_RTMP_URL="$3" KICK_STREAM_KEY="$4" KICK_DESTINO="${5:-}" \
    bash -c '. config/kick-destino.sh; kick_destino'); s=$?
  if [ "$s" = 0 ] && [ "$r" = "$2" ]; then echo "ok    $1"; else echo "FALLA $1: esperaba '$2', salió '$r' (salida $s)"; fallas=$((fallas + 1)); fi
}
falla() { # nombre, URL, clave: tiene que salir con 78 y sin imprimir nada
  local r s
  r=$(env -i KICK_RTMP_URL="$2" KICK_STREAM_KEY="$3" bash -c '. config/kick-destino.sh; kick_destino'); s=$?
  if [ "$s" = 78 ] && [ -z "$r" ]; then echo "ok    $1"; else echo "FALLA $1: esperaba salida 78 sin texto, salió $s '$r'"; fallas=$((fallas + 1)); fi
}
I=rtmps://ingesta.global-contribute.live-video.net
caso "sin ruta, con barra: agrega :443/app" "$I:443/app/sk_x" "$I/" sk_x
caso "sin ruta ni barra: agrega :443/app" "$I:443/app/sk_x" "$I" sk_x
caso "con /app/: agrega solo :443" "$I:443/app/sk_x" "$I/app/" sk_x
caso "con /app sin barra" "$I:443/app/sk_x" "$I/app" sk_x
caso "completa: queda igual" "$I:443/app/sk_x" "$I:443/app/" sk_x
caso "otro puerto: lo respeta" "$I:4443/app/sk_x" "$I:4443" sk_x
caso "otra ruta: la respeta" "$I:443/live/sk_x" "$I/live/" sk_x
caso "rtmp sin TLS: sin puerto agregado" "rtmp://127.0.0.1:1935/kick-humo/k" "rtmp://127.0.0.1:1935/kick-humo" k
caso "KICK_DESTINO manda" "rtmp://127.0.0.1:1935/kick-humo" "" "" "rtmp://127.0.0.1:1935/kick-humo"
falla "sin URL" "" sk_x
falla "sin clave" "$I/" ""
falla "esquema que no es rtmp(s)" "https://kick.com/canal" sk_x
if [ "$fallas" -gt 0 ]; then echo "$fallas falla(s)"; exit 1; fi
echo "todos los tests pasan"
