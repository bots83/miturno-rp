# Destino de la salida a Kick (lo lee miturno-kick-out.service). Se instala en
# /usr/local/lib/miturno/kick-destino.sh con scripts/video/desplegar-video.sh (repo principal).
#
# kick_destino imprime la URL de publicación a partir del entorno:
# - KICK_DESTINO (drop-in de la prueba de humo), tal cual;
# - si no, KICK_RTMP_URL + KICK_STREAM_KEY de /etc/miturno-video/kick.env. Kick usa Amazon IVS, que
#   espera rtmps://<ingesta>:443/app/<clave>. Kick muestra la URL a veces sin ruta
#   (rtmps://<ingesta>/): con la clave pegada directo, IVS acepta TLS y el saludo RTMP pero corta al
#   publicar. Por eso, si falta la ruta se agrega /app, y en rtmps sin puerto se agrega :443.
# Sale con 78 (configuración, como EX_CONFIG) y sin imprimir nada si falta algo o el esquema no es
# rtmp/rtmps. Nunca imprime la clave a otro lado que la salida estándar (la usa la unidad).
kick_destino() {
  if [ -n "${KICK_DESTINO:-}" ]; then
    printf '%s\n' "$KICK_DESTINO"
    return 0
  fi
  [ -n "${KICK_RTMP_URL:-}" ] && [ -n "${KICK_STREAM_KEY:-}" ] || return 78
  local esquema resto host ruta
  case $KICK_RTMP_URL in
    rtmps://* | rtmp://*) ;;
    *) return 78 ;;
  esac
  esquema=${KICK_RTMP_URL%%://*}
  resto=${KICK_RTMP_URL#*://}
  host=${resto%%/*}
  if [ "$host" = "$resto" ]; then ruta=; else ruta=/${resto#*/}; fi
  while [ "${ruta%/}" != "$ruta" ]; do ruta=${ruta%/}; done
  [ -n "$ruta" ] || ruta=/app
  [ -n "$host" ] || return 78
  case $host in
    *:*) ;;
    *) [ "$esquema" = rtmps ] && host=$host:443 ;;
  esac
  printf '%s://%s%s/%s\n' "$esquema" "$host" "$ruta" "$KICK_STREAM_KEY"
}
