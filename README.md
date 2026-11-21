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

- `rp.toml` (plantilla en `config/rp.toml`, sin secretos): bitrate, destino UDP del MPEG-TS, dirección HTTP local, cartel, clave pública y token de control.
- `casa.env`: `PS5_HOST` (nombre de la conexión de casa; privado, no se registra).
- `ps5.env` (600): `PS5_REGIST_KEY`, `PS5_MORNING`, `PS5_ACCOUNT_ID` (secretos; nunca se imprimen).

Servidor local (en `http` de `rp.toml`, por defecto `127.0.0.1:7070`; Caddy publica solo `/ctl` y `/control/*`):

- `GET /ctl` (WebSocket): mandos. Primer mensaje `{"t":"auth","token":…}` en menos de 2 s; un solo controlador a la vez (el nuevo de rol igual o mayor reemplaza al anterior); 500 ms sin mensajes, cierre, reemplazo o credencial vencida → todo suelto. Respuestas: `ok`, `error` (`FORMATO`, `FIRMA`, `EMISOR`, `AUDIENCIA`, `VENCIDA`, `ROL`, `OCUPADO`) y `reemplazado`.
- `POST /interno/auth` (autenticación externa de MediaMTX) y `POST /interno/keyframe`: solo desde la máquina y sin pasar por el proxy. Todos los pedidos de keyframe (este, el de `/ctl` y el del empalme al reconectar) pasan por un único limitador de 3 s; uno que cae dentro de la ventana no se pierde: sale al final de la ventana, y todos los de una misma ventana se juntan en un solo pedido diferido.
- `GET /control/salud` y `POST /control/{reconectar,kick_on,kick_off,reposo,encender}`: con `Authorization: Bearer <token de control>`. `reposo` pone la PS5 en reposo y deja el ciclo en pausa hasta `encender` (la salud informa `sesion.estado = "reposo"`). Al terminar cada conexión de `/ctl` se registran el rol y los toques que descartó el filtro.

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
