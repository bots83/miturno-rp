# miturno-rp

Cliente de Remote Play de [MiTurno](https://miturnolive.com): mantiene una sesión con una PS5, pasa su video **sin recomprimir** a un servidor de medios (MediaMTX) y recibe los mandos de quien juega por WebSocket, con una credencial firmada y un filtro de botones según el rol.

Corre en el servidor de video de MiTurno. **Este repo no tiene secretos ni datos de la red de casa:** la configuración real (registración de la consola, claves, nombre de la conexión) vive en `/etc/miturno-video/` del servidor y nunca se versiona.

## Licencia

[GNU AGPL-3.0](LICENSE). Si usás este programa para ofrecer un servicio por red, tenés que ofrecer su código fuente a quienes lo usan. Créditos de terceros en [NOTICE](NOTICE).

## Estructura

- `crates/rp-core`: lógica pura (credencial, mando, filtro por rol, "todo suelto", límite de keyframes, protocolo). Se prueba en cualquier máquina con `cargo test -p rp-core`.
- `crates/miturno-rp`: el binario. Enlaza libchiaki (chiaki-ng v1.10.1).
- `config/`: plantillas de configuración sin secretos, que instala el script de despliegue del servidor de video:
  - `rp.toml` (miturno-rp) y `miturno-rp.service` (usuario `rp`, sin privilegios; cierre con SIGTERM);
  - `mediamtx.yml` y `mediamtx.service` (MediaMTX v1.21.3: ruta `ps5` desde el MPEG-TS local, WHEP detrás de Caddy, RTSP y RTMP solo locales, autenticación por `/interno/auth`);
  - `miturno-kick-out.service` (ffmpeg hacia Kick a 720p60; arranca apagado, el destino sale de `kick.env` o de `KICK_DESTINO`);
  - `50-miturno-kick.rules` (polkit: el usuario `rp` solo puede arrancar, parar y reiniciar `miturno-kick-out.service`);
  - `Caddyfile` (TLS, WHEP con CORS para `https://miturnolive.com`, `/ctl` y `/control/*`; lo demás, 404) y `caddy-miturno.conf` (drop-in: la API de administración de Caddy por un socket unix en `/run/caddy`, 0700, en vez de TCP).

  La clave de Kick va en los argumentos de ffmpeg y `/proc` no está montado con `hidepid`: la ven todos los procesos de otros usuarios salvo los que corren con `ProtectProc=invisible`. Lo llevan los servicios expuestos a la red: miturno-rp, MediaMTX, Caddy y coturn (los dos últimos por un drop-in del script de despliegue). `ProtectProc` protege a quien lo lleva, no a ffmpeg.
- `scripts/`: herramientas del repo, incluido el chequeo de secretos que corre antes de cada push.

## Correr

```bash
miturno-rp [--config /etc/miturno-video/rp.toml] [--casa /etc/miturno-video/casa.env] [--ps5 /etc/miturno-video/ps5.env]
```

- `rp.toml` (plantilla en `config/rp.toml`, sin secretos): bitrate, destino UDP del MPEG-TS, dirección HTTP local, cartel, clave pública, token de control y `pausa` (opcional; por defecto `/var/lib/miturno-rp/pausa`, ver abajo).
- `casa.env`: `PS5_HOST` (nombre de la conexión de casa; privado, no se registra).
- `ps5.env` (600): `PS5_REGIST_KEY`, `PS5_MORNING`, `PS5_ACCOUNT_ID` (secretos; nunca se imprimen).

Servidor local (en `http` de `rp.toml`, por defecto `127.0.0.1:7070`; Caddy publica solo `/ctl` y `/control/*`):

- `GET /ctl` (WebSocket): mandos. Primer mensaje `{"t":"auth","token":…}` en menos de 2 s; un solo controlador a la vez (el nuevo de rol igual o mayor reemplaza al anterior); 500 ms sin mensajes, cierre, reemplazo o credencial vencida → todo suelto. Respuestas: `ok`, `error` (`FORMATO`, `FIRMA`, `EMISOR`, `AUDIENCIA`, `VENCIDA`, `ROL`, `OCUPADO`) y `reemplazado`.
- `POST /interno/auth` (autenticación externa de MediaMTX) y `POST /interno/keyframe`: solo desde la máquina y sin pasar por el proxy. Todos los pedidos de keyframe (este, el de `/ctl` y el del empalme al reconectar) pasan por un único limitador de 3 s; uno que cae dentro de la ventana no se pierde: sale al final de la ventana, y todos los de una misma ventana se juntan en un solo pedido diferido.
- `GET /control/salud` y `POST /control/{reconectar,kick_on,kick_off,reposo,encender}`: con `Authorization: Bearer <token de control>`. `reposo` pone la PS5 en reposo (si hay sesión) y deja el ciclo en pausa hasta `encender` (la salud informa `sesion.estado = "reposo"`). Con la consola tomada (ver abajo) no hay sesión: `reposo` **no apaga la PS5** (alguien la está usando); solo cambia la pausa a `reposo`, y desde ahí hace falta `encender` (que la despierta y la retoma). Si la PS5 cierra la sesión ella misma ("Server shutting down", por ejemplo cuando alguien prende el mando físico y elige perfil), miturno-rp no la retoma sola: muestra el cartel, la salud informa `sesion.estado = "tomada"` y espera `reconectar` (o `encender`). Valores de `sesion.estado`: `conectada`, `conectando`, `caida`, `reposo` y `tomada`. Al terminar cada conexión de `/ctl` se registran el rol y los toques que descartó el filtro.

Reintentos (`rp-core/src/reintento.rs`): por red, sin respuesta o sin video, 1, 2, 4 y después cada 5 s; con "Remote is already in use", cada 3 s sin despertar la PS5 y, si sigue así más de 2 min (40 seguidos), cada 15 s, con el aviso del registro limitado a uno por minuto; por otros motivos (versión, PIN, Remote Play caído en la consola), creciente hasta 30 s. La red y los otros motivos tienen cada uno su escalón (muchos cortes de red no hacen esperar 30 s al primer "otro motivo", ni al revés). `CTRL_UNKNOWN` de libchiaki cuenta como red. Después de una sesión de 30 s o más, o con `reconectar`/`encender`, la espera vuelve a 1 s.

### Consola tomada, reposo y reinicios

Las dos pausas (`tomada` y `reposo`) se guardan en el archivo `pausa` de `rp.toml` (por defecto `/var/lib/miturno-rp/pausa`, el `StateDirectory` de `miturno-rp.service`, rp:rp 0700). Se escribe de forma atómica (temporal, `fsync` y `rename`) al entrar en la pausa y se borra al salir (`reconectar` o `encender`). Al arrancar, si existe, miturno-rp empieza en esa pausa y no despierta ni conecta: un reinicio (pánico con `Restart=always`, despliegue o reinicio del servidor) no retoma la consola ni despierta una PS5 en reposo. Si el archivo está corrupto, se registra, se borra y se arranca como sin pausa; si no se puede leer, se registra y se arranca como sin pausa; si no se puede escribir, la pausa vale igual en memoria y se registra que un reinicio no la recordaría. Para que retome al arrancar sin pasar por el control, alcanza con borrar el archivo con miturno-rp parado.

`tomada` sale solo cuando la PS5 cierra la sesión con `REMOTE_SHUTDOWN` ("Server shutting down"). Medido: alguien prende el mando físico y elige perfil. También lo producen, y quedan igual en `tomada` (decisión del dueño: en todos esos casos alguien tocó la consola y no corresponde retomarla sola), mandar la PS5 a reposo desde la consola, desactivar la red desde los ajustes de la PS5, una actualización del sistema y apagarla. Recomendación: en la PS5, no entrar en reposo mientras se juega y actualizar el sistema solo con la consola en reposo.

Límites conocidos:

- Si alguien toma la consola mientras miturno-rp no tiene sesión (esperando el próximo intento o en el bucle de "Remote is already in use"), no llega `REMOTE_SHUTDOWN` y el próximo intento la retoma.
- Un `REMOTE_SHUTDOWN` que llega con el ciclo esperando el próximo intento (de una sesión que ya se dio por caída) se ignora: solo cuenta el de la sesión abierta o la que está conectando.

Sin sesión, la salida repite el cartel "Volvemos enseguida" (`/var/lib/miturno-video/cartel.ts`); al conectar, empalma en el primer keyframe de la PS5. Las marcas de tiempo son propias y continuas. Ante SIGTERM cierra la sesión con `session.stop()` antes de salir. `MITURNO_RP_DEPURAR=1` activa el registro de depuración.

## Compilar

Requiere la librería libchiaki compilada con [nanpuyue/libchiaki](https://github.com/nanpuyue/libchiaki) (`build-libchiaki.sh`, `CHIAKI_VERSION=v1.10.1`):

```bash
LIBCHIAKI_PREFIX=/ruta/a/libchiaki-1.10.1 cargo build --release
```

La versión de Rust está fijada en `rust-toolchain.toml`.

## Antes de publicar

El hook `.git/hooks/pre-push` (no se versiona; si hace falta, se reinstala con este contenido y `chmod +x`) corre:

```bash
#!/usr/bin/env bash
set -e
cd "$(git rev-parse --show-toplevel)"
bash scripts/test-chequeo-sin-secretos.sh >/dev/null
bash scripts/chequeo-sin-secretos.sh --todo
bash scripts/chequeo-sin-secretos.sh --historial
bash scripts/test-chequeo-versiones.sh >/dev/null
bash scripts/chequeo-versiones.sh
```

`chequeo-versiones.sh` exige que cada paquete de `Cargo.lock`, también los transitivos, tenga 14 días o más desde su publicación. Si falla, se baja con `cargo update -p <crate>@<versión> --precise <anterior>`.
