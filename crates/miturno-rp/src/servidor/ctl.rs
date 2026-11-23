//! `GET /ctl`: el WebSocket de los mandos.
//!
//! - El primer mensaje tiene que ser `auth` en menos de 2 s; si no, `error FORMATO` y se cierra.
//!   Como mucho `MAX_SIN_AUTH` conexiones esperan `auth` a la vez, y `MAX_SIN_AUTH_POR_IP` de una
//!   misma IP (la del cliente que pasa Caddy en `X-Forwarded-For`): las demás reciben 503 sin upgrade.
//! - Con la credencial válida, `Autorizados::mandos` decide: el jugador solo manda si su `(sub, turnId)`
//!   es el turno vigente que empujó la api por `/control/turno` (si no, `error TURNO`) y la fila no está
//!   en pausa (`"mandos":false`: `error PAUSA`), la vista nunca (`error ROL`) y el operador siempre.
//!   Rechazada así, la conexión no toma la ranura: una vista nunca suelta lo que tenía el jugador.
//! - Un solo controlador a la vez: una conexión válida de rol igual o mayor reemplaza a la anterior
//!   (que recibe `reemplazado` con el rol de la nueva y se cierra); una de rol menor recibe `error OCUPADO`.
//! - Si `/control/turno` deja al jugador que controla sin turno, se le suelta todo y se libera la ranura
//!   antes de responder (con la ranura tomada, sin ventana), y la conexión recibe `error TURNO` y se cierra.
//!   Si lo deja sin mandos por la pausa, lo mismo con `error PAUSA`: el video (WHEP) no se toca.
//! - La actividad del jugador (`Actividad`) se mide con lo que llega a la PS5: cada `estado` aplicado
//!   se registra, y el todo suelto (por silencio, reemplazo, corte o fin de la conexión) la suelta.
//! - Cada mensaje válido es un latido. Con 500 ms de silencio se suelta todo (la conexión sigue); al
//!   cerrar, al llegar un mensaje inválido, al ser reemplazada o al vencer la credencial, también.
//!   Si la conexión vuelve después de un silencio (un hipo de red), el primer mensaje válido que no
//!   sea `estado` vuelve a aplicar la última entrada filtrada: un botón mantenido sigue mantenido.
//!   Si el jugador soltó durante el hueco, su `estado` nuevo llega antes (TCP entrega en orden) y manda.
//! - El estado del mando pasa por el filtro del rol (el jugador nunca pasa PS ni Create).
//! - Los pedidos de keyframe pasan por el limitador único (`ConLimite`).
//!
//! Solo el controlador actual llega a `poner_mando`, y siempre con la ranura tomada: así el "todo
//! suelto" de una conexión vieja nunca pisa a la nueva.
//! La ranura la libera un guardián (`Dueno`) al soltarse: también si la tarea entra en pánico o se
//! cancela. Al terminar se registran el rol y los toques que descartó el filtro (nunca el token).

use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::{close_code, CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use rp_core::{
    leer_mensaje, resumir, validar_con, Actividad, Autorizados, Credencial, EstadoMando, Filtro, Guardian, MsgCliente,
    MsgServidor, ResumenMando, Rol, SinCambios,
};
use tokio::sync::{oneshot, OwnedSemaphorePermit, Semaphore};
use tokio::time::{sleep_until, timeout, Instant};

use super::whep::SesionesWhep;
use super::EstadoServidor;
use crate::sesion::SesionPs5;

/// Plazo para el mensaje `auth`.
pub const PLAZO_AUTH: Duration = Duration::from_secs(2);
/// Silencio (sin mensajes ni latidos) tras el que se suelta todo.
pub const SILENCIO_MS: u64 = 500;
/// Tope de una conexión aunque la credencial venza más tarde: lo que dura la credencial más larga
/// que emite la api (1 h, `CREDENCIAL_TTL_S`) más la tolerancia de reloj. Así un `exp` largo por un
/// error de la api no da más control, y un `exp` enorme no desborda el reloj. Si la api emite
/// credenciales más largas, hay que subirlo.
pub const TOPE_CONEXION: Duration = Duration::from_secs(3600 + 30);
/// Conexiones que pueden estar a la vez esperando `auth` (las demás reciben 503 antes del upgrade).
/// /ctl es público por Caddy: sin tope, muchas conexiones que nunca se autentican se juntan sin límite.
/// Es el tope de memoria; a una IP sola la frena `MAX_SIN_AUTH_POR_IP`.
pub const MAX_SIN_AUTH: usize = 512;
/// Conexiones de una misma IP (IPv6: de un mismo /64) que pueden estar a la vez esperando `auth`. Con
/// un tope global solo, una IP que abre ~16 conexiones por segundo (cada una ocupa su lugar hasta
/// `PLAZO_AUTH`) dejaba afuera al operador. Una conexión legítima lo ocupa unos milisegundos.
pub const MAX_SIN_AUTH_POR_IP: usize = 4;
/// Tope de un mensaje (una credencial ocupa menos de 1 KiB).
const MAX_MENSAJE: usize = 4 * 1024;
/// Plazo para mandar el último mensaje antes de cerrar (un cliente que no lee no traba la tarea).
const PLAZO_ENVIO: Duration = Duration::from_secs(1);

/// Los rechazos de `/ctl` por turno (`TURNO`), por pausa (`PAUSA`) o por rol (`ROL`): uno por minuto en
/// el registro.
pub(super) static RECHAZO_MANDOS: crate::registro::Limitado = crate::registro::Limitado::new();

/// Los rechazos `OCUPADO` de `/ctl`: uno por minuto en el registro.
pub(super) static RECHAZO_OCUPADO: crate::registro::Limitado = crate::registro::Limitado::new();

/// Por qué una conexión pierde la ranura (el aviso que recibe).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Expulsion {
    /// Otra conexión de rol igual o mayor tomó el mando.
    Reemplazada { por: Rol },
    /// El turno de este jugador dejó de ser el vigente (`/control/turno`).
    Revocada,
    /// La fila está en pausa: el turno sigue, pero sin mandos (`/control/turno` con `"mandos":false`).
    Pausa,
}

pub(super) struct Ocupante {
    id: u64,
    rol: Rol,
    sub: String,
    turn_id: Option<String>,
    aviso: oneshot::Sender<Expulsion>,
}

impl Ocupante {
    /// El turno del jugador que controla (la actividad solo mide jugadores).
    fn turno_jugador(&self) -> Option<&str> {
        (self.rol == Rol::Jugador).then_some(self.turn_id.as_deref()).flatten()
    }

    /// Si estos autorizados ya no le dan mandos, por qué se lo corta (`None`: sigue). La regla es la de
    /// `/ctl` (`Autorizados::mandos_de`, que usa también `Autorizados::mandos`).
    fn corte(&self, a: &Autorizados) -> Option<Expulsion> {
        match a.mandos_de(self.rol, &self.sub, self.turn_id.as_deref()) {
            Ok(()) => None,
            Err("PAUSA") => Some(Expulsion::Pausa),
            Err(_) => Some(Expulsion::Revocada),
        }
    }
}

/// Lo que protege el mismo candado: quién controla, quién puede controlar o ver, la actividad del
/// jugador y las sesiones WHEP abiertas. Con el mismo candado, un WHEP que se autoriza con el turno
/// viejo no puede registrarse después de que `/control/turno` calculó a quién expulsar.
/// Al arrancar no hay nadie autorizado ni actividad; las sesiones WHEP vienen del registro guardado.
pub(super) struct Ranura {
    pub(super) ocupante: Option<Ocupante>,
    autorizados: Autorizados,
    actividad: Actividad,
    sin_cambios: SinCambios,
    whep: SesionesWhep,
}

impl Ranura {
    pub(super) fn new(whep: SesionesWhep) -> Self {
        Self {
            ocupante: None,
            autorizados: Autorizados::vacio(),
            actividad: Actividad::new(),
            sin_cambios: SinCambios::new(),
            whep,
        }
    }
}

/// De dónde viene un estado que se manda a la PS5: lo que cambia el mando del jugador (`Jugador`, un
/// mensaje `estado` real) o algo que arma el servidor (`Sintetico`: el todo suelto por silencio y su
/// re-aplicación al volver). Solo el primero cuenta para `SinCambios`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Origen {
    Jugador,
    Sintetico,
}

/// `/control/turno` con una versión menor que la guardada (que va adentro).
#[derive(Debug, PartialEq, Eq)]
pub(super) struct VersionVieja(pub(super) u64);

/// Lo que hizo `/control/turno`: la versión que había, la conexión que cortó y por qué (`Revocada` o
/// `Pausa`), si hubo, y las sesiones WHEP que ya no tienen video (para expulsar).
#[derive(Debug, PartialEq, Eq)]
pub(super) struct TurnoAplicado {
    pub(super) anterior: u64,
    pub(super) cortada: Option<(u64, Expulsion)>,
    pub(super) expulsar: Vec<String>,
}

/// Lugares para las conexiones que todavía no se autenticaron: el tope global y el de cada IP.
pub(super) struct SinAuth {
    global: Arc<Semaphore>,
    /// Lugares ocupados por IP; una IP sin lugares ocupados no queda en el mapa (como mucho
    /// `MAX_SIN_AUTH` entradas).
    por_ip: Mutex<HashMap<IpAddr, usize>>,
}

/// Un lugar de `SinAuth`. Se devuelve al soltarlo, en cualquier camino: autenticada (bien o mal),
/// plazo vencido, cierre antes de `auth`, upgrade fallido (axum suelta la clausura sin llamarla),
/// pánico o tarea cancelada.
pub(super) struct Lugar {
    sin_auth: Arc<SinAuth>,
    ip: IpAddr,
    _global: OwnedSemaphorePermit,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum SinLugar {
    Global,
    PorIp,
}

impl SinAuth {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self { global: Arc::new(Semaphore::new(MAX_SIN_AUTH)), por_ip: Mutex::new(HashMap::new()) })
    }

    fn con_mapa<R>(&self, f: impl FnOnce(&mut HashMap<IpAddr, usize>) -> R) -> R {
        f(&mut self.por_ip.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Toma un lugar para una conexión de `ip` (ya agrupada con `clave_ip`).
    pub(super) fn tomar(self: &Arc<Self>, ip: IpAddr) -> Result<Lugar, SinLugar> {
        self.con_mapa(|m| {
            let n = m.get(&ip).copied().unwrap_or(0);
            if n >= MAX_SIN_AUTH_POR_IP {
                return Err(SinLugar::PorIp);
            }
            let global = self.global.clone().try_acquire_owned().map_err(|_| SinLugar::Global)?;
            m.insert(ip, n + 1);
            Ok(Lugar { sin_auth: self.clone(), ip, _global: global })
        })
    }

    /// (lugares globales ocupados, IP con algún lugar ocupado).
    #[cfg(test)]
    pub(super) fn ocupados(&self) -> (usize, usize) {
        (MAX_SIN_AUTH - self.global.available_permits(), self.con_mapa(|m| m.len()))
    }
}

impl Drop for Lugar {
    fn drop(&mut self) {
        let ip = self.ip;
        self.sin_auth.con_mapa(|m| {
            match m.get_mut(&ip) {
                Some(n) if *n > 1 => *n -= 1,
                _ => {
                    m.remove(&ip);
                }
            }
        });
    }
}

/// La IP del cliente. Si el par es la propia máquina (Caddy), la última de `X-Forwarded-For`: Caddy
/// (sin `trusted_proxies`) descarta la que manda el cliente y pone la del par TCP, y si alguna vez
/// confiara en otro proxy, la agregaría al final. Si el par no es local, o el encabezado falta o no
/// se entiende, la del par: nadie de afuera elige con qué IP cuenta.
pub(super) fn ip_cliente(par: SocketAddr, h: &HeaderMap) -> IpAddr {
    let ip_par = par.ip().to_canonical();
    if !ip_par.is_loopback() {
        return ip_par;
    }
    h.get_all("x-forwarded-for")
        .iter()
        .next_back()
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.rsplit(',').next())
        .and_then(|v| v.trim().parse::<IpAddr>().ok())
        .map(|ip| ip.to_canonical())
        .unwrap_or(ip_par)
}

/// La clave del tope por IP: la IPv4 entera; de una IPv6, su /64 (lo que tiene una sola conexión de casa).
pub(super) fn clave_ip(ip: IpAddr) -> IpAddr {
    match ip.to_canonical() {
        IpAddr::V6(v6) => IpAddr::V6(Ipv6Addr::from(u128::from(v6) & !((1u128 << 64) - 1))),
        v4 => v4,
    }
}

impl<S: SesionPs5> EstadoServidor<S> {
    fn ahora_ms(&self) -> u64 {
        self.t0.elapsed().as_millis() as u64
    }

    fn con_ranura<R>(&self, f: impl FnOnce(&mut Ranura) -> R) -> R {
        f(&mut self.ranura.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Toma el control si la credencial tiene mandos (`TURNO`/`PAUSA`/`ROL` si no) y no hay nadie o el
    /// actual tiene un rol igual o menor (`OCUPADO` si no). Rechazada, no toca la ranura ni la PS5. Si no,
    /// suelta lo que haya dejado el anterior y le avisa. Devuelve el guardián de la ranura (ya armado,
    /// sin ventana en la que un pánico la deje tomada) y el aviso de expulsión de esta conexión.
    /// Todo con la ranura tomada: un `/control/turno` no puede colarse entre la autorización y la toma.
    pub(super) fn tomar(
        self: &Arc<Self>,
        id: u64,
        cred: &Credencial,
    ) -> Result<(Dueno<S>, oneshot::Receiver<Expulsion>), &'static str> {
        let ahora = self.ahora_ms();
        let rx = self.con_ranura(|r| {
            r.autorizados.mandos(cred)?;
            if r.ocupante.as_ref().is_some_and(|o| cred.rol < o.rol) {
                return Err("OCUPADO");
            }
            if let Some(viejo) = r.ocupante.take() {
                crate::info!("ctl: la conexión {id} reemplaza a la {}", viejo.id);
                if let Some(t) = viejo.turno_jugador() {
                    r.actividad.soltar(t, ahora);
                }
                let _ = viejo.aviso.send(Expulsion::Reemplazada { por: cred.rol });
            }
            let (tx, rx) = oneshot::channel();
            r.ocupante =
                Some(Ocupante { id, rol: cred.rol, sub: cred.sub.clone(), turn_id: cred.turn_id.clone(), aviso: tx });
            self.sesion.poner_mando(&EstadoMando::suelto());
            self.publicar_mando(r.ocupante.as_ref().and_then(Ocupante::turno_jugador), &EstadoMando::suelto());
            Ok(rx)
        })?;
        Ok((Dueno { e: self.clone(), id }, rx))
    }

    /// Publica el resumen del mando que recibe la PS5 (`None`: no controla un jugador, todo en cero),
    /// solo si cambió. Se llama con la ranura tomada: el orden de las publicaciones es el de las órdenes.
    fn publicar_mando(&self, turn_id: Option<&str>, estado: &EstadoMando) {
        let nuevo = resumir(turn_id, estado);
        self.mando_tx.send_if_modified(|actual: &mut ResumenMando| {
            let cambio = *actual != nuevo;
            if cambio {
                *actual = nuevo;
            }
            cambio
        });
    }

    /// Pasa el estado a la PS5 solo si esta conexión es la que controla; si es un jugador, lo registra
    /// en la actividad (un estado sin actividad, como el todo suelto por silencio, equivale a
    /// `Actividad::soltar`). `SinCambios` solo registra lo que `Origen::Jugador` dice que mandó el
    /// propio jugador: el todo suelto por silencio y su re-aplicación son `Origen::Sintetico`.
    pub(super) fn mandar(&self, id: u64, estado: &EstadoMando, origen: Origen) {
        let ahora = self.ahora_ms();
        self.con_ranura(|r| {
            if let Some(o) = r.ocupante.as_ref().filter(|o| o.id == id) {
                self.sesion.poner_mando(estado);
                if let Some(t) = o.turno_jugador() {
                    self.publicar_mando(Some(t), estado);
                    r.actividad.registrar(t, estado, ahora);
                    if origen == Origen::Jugador {
                        r.sin_cambios.registrar(t, estado, ahora);
                    }
                }
            }
        });
    }

    /// Suelta todo y libera la ranura, si esta conexión todavía la tiene.
    pub(super) fn soltar(&self, id: u64) {
        let ahora = self.ahora_ms();
        self.con_ranura(|r| {
            if let Some(o) = r.ocupante.as_ref().filter(|o| o.id == id) {
                self.sesion.poner_mando(&EstadoMando::suelto());
                if let Some(t) = o.turno_jugador() {
                    r.actividad.soltar(t, ahora);
                }
                r.ocupante = None;
                self.publicar_mando(None, &EstadoMando::suelto());
            }
        });
    }

    /// Guarda el turno vigente de `/control/turno` (una versión igual se acepta: el empuje es
    /// idempotente) y fija el turno de la actividad. Con la misma ranura tomada, si el que controla
    /// ya no tiene mandos (otro turno o la fila en pausa), le suelta todo, libera la ranura y le avisa
    /// por qué: cuando esto vuelve, la PS5 ya no recibe nada de él.
    pub(super) fn aplicar_turno(&self, a: Autorizados) -> Result<TurnoAplicado, VersionVieja> {
        let ahora = self.ahora_ms();
        self.con_ranura(|r| {
            let anterior = r.autorizados.version;
            if a.version < anterior {
                return Err(VersionVieja(anterior));
            }
            r.autorizados = a;
            let turno = r.autorizados.jugador.as_ref().map(|j| j.turn_id.as_str());
            r.actividad.turno(turno, ahora);
            r.sin_cambios.turno(turno, ahora);
            let expulsar = r.whep.no_autorizadas(&r.autorizados);
            let corte = r.ocupante.as_ref().and_then(|o| o.corte(&r.autorizados));
            let (Some(motivo), Some(o)) = (corte, r.ocupante.take_if(|_| corte.is_some())) else {
                return Ok(TurnoAplicado { anterior, cortada: None, expulsar });
            };
            self.sesion.poner_mando(&EstadoMando::suelto());
            if let Some(t) = o.turno_jugador() {
                r.actividad.soltar(t, ahora);
            }
            self.publicar_mando(None, &EstadoMando::suelto());
            let _ = o.aviso.send(motivo);
            Ok(TurnoAplicado { anterior, cortada: Some((o.id, motivo)), expulsar })
        })
    }

    /// Un WHEP de `ps5` con una credencial ya validada: ¿lo deja ver el turno vigente
    /// (`Autorizados::video`)? Si sí, registra la sesión `id` (si es un UUID) con el mismo candado que
    /// los autorizados y devuelve las anteriores de la misma cuenta, para expulsarlas. `None`: no ve.
    pub(super) fn autorizar_whep(&self, c: &Credencial, id: Option<&str>) -> Option<Vec<String>> {
        self.con_ranura(|r| {
            if !r.autorizados.video(c.rol, &c.sub, c.turn_id.as_deref()) {
                return None;
            }
            let expulsar = id.map(|id| r.whep.registrar(id, c.rol, &c.sub, c.turn_id.as_deref())).unwrap_or_default();
            self.guardar_whep(r);
            Some(expulsar)
        })
    }

    /// Saca del registro una sesión WHEP ya expulsada.
    pub(super) fn quitar_whep(&self, id: &str) {
        self.con_ranura(|r| {
            r.whep.quitar(id);
            self.guardar_whep(r);
        });
    }

    /// La expulsión de esa sesión falló del todo: queda para el próximo intento.
    pub(super) fn fallida_whep(&self, id: &str) {
        self.con_ranura(|r| r.whep.fallida(id));
    }

    /// Pasa la versión actual del registro al que lo guarda en disco (solo si cambió; lo pendiente no
    /// cuenta). La escritura es fuera del candado.
    fn guardar_whep(&self, r: &Ranura) {
        if let Some(tx) = &self.guardar_whep {
            let json = r.whep.a_json();
            tx.send_if_modified(|v| {
                let cambio = *v != json;
                if cambio {
                    *v = json;
                }
                cambio
            });
        }
    }

    /// `VideoSalud.jugador`: el turno vigente guardado, quién controla y la inactividad del jugador.
    pub(super) fn salud_jugador(&self) -> super::SaludJugador {
        let ahora = self.ahora_ms();
        self.con_ranura(|r| super::SaludJugador {
            version: (r.autorizados.version != 0).then_some(r.autorizados.version),
            turn_id: r.autorizados.jugador.as_ref().map(|j| j.turn_id.clone()),
            mando: r.ocupante.as_ref().and_then(|o| match o.rol {
                Rol::Jugador => Some("jugador"),
                Rol::Operador => Some("operador"),
                Rol::Vista => None,
            }),
            inactivo_ms: r.actividad.inactivo_ms(ahora),
            sin_cambios_ms: r.sin_cambios.ms(ahora),
        })
    }
}

/// Guardián de la ranura: al soltarlo (fin normal, pánico o tarea cancelada) suelta todo y libera la
/// ranura, si esta conexión todavía la tiene. Así nunca queda la PS5 con botones apretados.
pub(super) struct Dueno<S: SesionPs5> {
    e: Arc<EstadoServidor<S>>,
    id: u64,
}

impl<S: SesionPs5> Drop for Dueno<S> {
    fn drop(&mut self) {
        self.e.soltar(self.id);
    }
}

pub(super) async fn ctl<S: SesionPs5>(
    State(e): State<Arc<EstadoServidor<S>>>,
    ConnectInfo(par): ConnectInfo<SocketAddr>,
    encabezados: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    // El lugar se toma antes del upgrade y se suelta al terminar la autenticación (o si el upgrade falla).
    let lugar = match e.sin_auth.tomar(clave_ip(ip_cliente(par, &encabezados))) {
        Ok(l) => l,
        Err(motivo) => {
            // Sin la IP en el registro.
            static GLOBAL: crate::registro::Limitado = crate::registro::Limitado::new();
            static POR_IP: crate::registro::Limitado = crate::registro::Limitado::new();
            match motivo {
                SinLugar::Global => crate::limitado!(4, GLOBAL, "ctl: {MAX_SIN_AUTH} conexiones esperando auth; rechazo otra (503)"),
                SinLugar::PorIp => crate::limitado!(
                    4,
                    POR_IP,
                    "ctl: una IP ya tiene {MAX_SIN_AUTH_POR_IP} conexiones esperando auth; rechazo otra (503)"
                ),
            }
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };
    ws.max_message_size(MAX_MENSAJE).max_frame_size(MAX_MENSAJE).on_upgrade(move |socket| conexion(e, socket, lugar))
}

enum Leido {
    Mensaje(MsgCliente),
    Invalido,
    Cerrada,
}

/// El próximo mensaje del cliente (los ping/pong los contesta axum). Se puede cancelar sin perder nada.
async fn leer(socket: &mut WebSocket) -> Leido {
    loop {
        return match socket.recv().await {
            None | Some(Err(_)) | Some(Ok(Message::Close(_))) => Leido::Cerrada,
            Some(Ok(Message::Text(t))) => match leer_mensaje(t.as_str()) {
                Ok(m) => Leido::Mensaje(m),
                Err(_) => Leido::Invalido,
            },
            Some(Ok(Message::Binary(_))) => Leido::Invalido,
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
        };
    }
}

async fn enviar(socket: &mut WebSocket, m: &MsgServidor) -> bool {
    matches!(timeout(PLAZO_ENVIO, socket.send(Message::Text(m.a_json().into()))).await, Ok(Ok(())))
}

/// Manda el último mensaje (si hay) y cierra.
async fn despedir(mut socket: WebSocket, m: Option<MsgServidor>) {
    if let Some(m) = m {
        if !enviar(&mut socket, &m).await {
            return;
        }
    }
    let cierre = Message::Close(Some(CloseFrame { code: close_code::NORMAL, reason: "".into() }));
    let _ = timeout(PLAZO_ENVIO, socket.send(cierre)).await;
}

fn nombre_rol(rol: Rol) -> &'static str {
    match rol {
        Rol::Operador => "operador",
        Rol::Jugador => "jugador",
        Rol::Vista => "vista",
    }
}

enum Fin {
    Cerrada,
    Formato,
    Vencida,
    Reemplazada(Rol),
    /// `/control/turno` dejó a este jugador sin turno (ya se soltó todo y se liberó la ranura).
    Revocada,
    /// `/control/turno` puso la fila en pausa: el mismo corte que `Revocada`, con otro aviso.
    Pausa,
}

async fn conexion<S: SesionPs5>(e: Arc<EstadoServidor<S>>, mut socket: WebSocket, lugar: Lugar) {
    let id = e.proximo_id.fetch_add(1, Ordering::Relaxed);
    // El token vive solo en este brazo: se valida (con la hora de cuando llegó) y se suelta.
    let (validada, ahora_s) = match timeout(PLAZO_AUTH, leer(&mut socket)).await {
        Ok(Leido::Mensaje(MsgCliente::Auth { token })) => {
            let ahora_s = (e.reloj_s)();
            (validar_con(&token, &e.clave, ahora_s), ahora_s)
        }
        Ok(Leido::Cerrada) => return,
        _ => {
            drop(lugar);
            return despedir(socket, Some(MsgServidor::error("FORMATO"))).await;
        }
    };
    // Terminó la autenticación (bien o mal): el lugar queda para otra.
    drop(lugar);
    let cred = match validada {
        Ok(c) => c,
        Err(err) => {
            static LIM: crate::registro::Limitado = crate::registro::Limitado::new();
            crate::limitado!(6, LIM, "ctl: credencial rechazada ({})", err.codigo());
            return despedir(socket, Some(MsgServidor::error(err.codigo()))).await;
        }
    };
    let (dueno, mut aviso) = match e.tomar(id, &cred) {
        Ok(t) => t,
        Err(codigo) => {
            // `TURNO` y `ROL` los puede provocar cualquiera con una credencial válida (también se
            // esperan en la carrera del arranque del turno), `PAUSA` el jugador que reintenta cada 2 s
            // durante la pausa y `OCUPADO`, el jugador que reintenta con el operador al mando: limitados,
            // como la credencial rechazada (cada uno con su límite).
            if codigo == "OCUPADO" {
                crate::limitado!(6, RECHAZO_OCUPADO, "ctl: conexión {id} (rol={}) rechazada ({codigo})", nombre_rol(cred.rol));
            } else {
                crate::limitado!(6, RECHAZO_MANDOS, "ctl: conexión {id} (rol={}) rechazada ({codigo})", nombre_rol(cred.rol));
            }
            return despedir(socket, Some(MsgServidor::error(codigo))).await;
        }
    };
    // `exp` es hora de pared: se pasa una sola vez a un plazo del reloj monótono, con tope (un `exp`
    // enorme desbordaría el `Instant`).
    let vence = Instant::now() + Duration::from_secs(cred.vence_s().saturating_sub(ahora_s)).min(TOPE_CONEXION);
    if !enviar(&mut socket, &MsgServidor::Ok { rol: cred.rol }).await {
        return;
    }
    crate::info!("ctl: conexión {id} (rol={}) controla la PS5", nombre_rol(cred.rol));

    let mut filtro = Filtro::new(cred.rol);
    let fin = bucle(&e, id, &mut filtro, &mut socket, &mut aviso, vence).await;
    // Primero se suelta todo (si todavía controla) y recién después se habla con el cliente.
    drop(dueno);
    let descartados = filtro.descartados();
    e.descartados.fetch_add(descartados, Ordering::Relaxed);
    let (motivo, ultimo) = match fin {
        Fin::Cerrada => ("cerrada", None),
        Fin::Formato => ("mensaje inválido", Some(MsgServidor::error("FORMATO"))),
        Fin::Vencida => ("credencial vencida", Some(MsgServidor::error("VENCIDA"))),
        Fin::Reemplazada(por) => ("reemplazada", Some(MsgServidor::Reemplazado { por })),
        Fin::Revocada => ("turno terminado", Some(MsgServidor::error("TURNO"))),
        Fin::Pausa => ("fila en pausa", Some(MsgServidor::error("PAUSA"))),
    };
    crate::info!("ctl: conexión {id} terminada ({motivo}; rol={}, descartados={descartados})", nombre_rol(cred.rol));
    if ultimo.is_some() {
        despedir(socket, ultimo).await;
    }
}

async fn bucle<S: SesionPs5>(
    e: &EstadoServidor<S>,
    id: u64,
    filtro: &mut Filtro,
    socket: &mut WebSocket,
    aviso: &mut oneshot::Receiver<Expulsion>,
    vence: Instant,
) -> Fin {
    let mut guardian = Guardian::new(SILENCIO_MS);
    let mut ultimo = e.ahora_ms();
    guardian.latido(ultimo);
    let mut suelto_por_silencio = false;
    // Lo último que pasó el filtro: se reaplica si la conexión vuelve después de un silencio.
    let mut ultima_filtrada = EstadoMando::suelto();
    let ms = |m: u64| e.t0 + Duration::from_millis(m);

    loop {
        let silencio = (!suelto_por_silencio).then(|| ms(ultimo + SILENCIO_MS));
        tokio::select! {
            biased;
            expulsion = &mut *aviso => return match expulsion {
                Ok(Expulsion::Reemplazada { por }) => Fin::Reemplazada(por),
                Ok(Expulsion::Pausa) => Fin::Pausa,
                // `Err`: la ranura soltó el aviso sin mandarlo (no debería pasar); igual se corta.
                Ok(Expulsion::Revocada) | Err(_) => Fin::Revocada,
            },
            _ = sleep_until(vence) => return Fin::Vencida,
            _ = sleep_until(silencio.unwrap_or(vence)), if silencio.is_some() => {
                let ahora = e.ahora_ms();
                if guardian.vencido(ahora) {
                    crate::depurar!("ctl: conexión {id}: {SILENCIO_MS} ms sin mensajes; todo suelto");
                    // Sin actividad: para la del jugador, `mandar` equivale a `Actividad::soltar`.
                    let salida = filtro.aplicar(&EstadoMando::suelto(), ahora);
                    e.mandar(id, &salida, Origen::Sintetico);
                    suelto_por_silencio = true;
                }
            }
            leido = leer(socket) => {
                let m = match leido {
                    Leido::Mensaje(m) => m,
                    Leido::Invalido => return Fin::Formato,
                    Leido::Cerrada => return Fin::Cerrada,
                };
                let ahora = e.ahora_ms();
                guardian.latido(ahora);
                ultimo = ahora;
                let volvio_del_silencio = std::mem::take(&mut suelto_por_silencio);
                if volvio_del_silencio && !matches!(m, MsgCliente::Estado { .. } | MsgCliente::Auth { .. }) {
                    crate::depurar!("ctl: conexión {id}: volvió tras el silencio; reaplico el último estado");
                    e.mandar(id, &ultima_filtrada, Origen::Sintetico);
                }
                match m {
                    MsgCliente::Auth { .. } => return Fin::Formato,
                    MsgCliente::Estado { botones, l2, r2, lx, ly, rx, ry } => {
                        let entrada = EstadoMando { botones, l2, r2, lx, ly, rx, ry };
                        let salida = filtro.aplicar(&entrada, ahora);
                        ultima_filtrada = salida;
                        e.mandar(id, &salida, Origen::Jugador);
                        #[cfg(test)]
                        if e.panico_en_estado.load(std::sync::atomic::Ordering::SeqCst) {
                            panic!("pánico de prueba en la conexión {id}");
                        }
                    }
                    MsgCliente::Latido {} => {}
                    MsgCliente::PedirKeyframe {} => {
                        let enviado = e.sesion.pedir_keyframe();
                        crate::depurar!("ctl: conexión {id}: keyframe {}", if enviado { "pedido" } else { "diferido" });
                    }
                }
            }
        }
    }
}
