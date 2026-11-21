//! `GET /ctl`: el WebSocket de los mandos.
//!
//! - El primer mensaje tiene que ser `auth` en menos de 2 s; si no, `error FORMATO` y se cierra.
//!   Como mucho `MAX_SIN_AUTH` conexiones esperan `auth` a la vez, y `MAX_SIN_AUTH_POR_IP` de una
//!   misma IP (la del cliente que pasa Caddy en `X-Forwarded-For`): las demás reciben 503 sin upgrade.
//! - Un solo controlador a la vez: una conexión válida de rol igual o mayor reemplaza a la anterior
//!   (que recibe `reemplazado` y se cierra); una de rol menor recibe `error OCUPADO`.
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
use rp_core::{leer_mensaje, validar_con, EstadoMando, Filtro, Guardian, MsgCliente, MsgServidor, Rol};
use tokio::sync::{oneshot, OwnedSemaphorePermit, Semaphore};
use tokio::time::{sleep_until, timeout, Instant};

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

pub(super) struct Ocupante {
    id: u64,
    rol: Rol,
    reemplazo: oneshot::Sender<()>,
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

    fn con_ranura<R>(&self, f: impl FnOnce(&mut Option<Ocupante>) -> R) -> R {
        f(&mut self.ranura.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Toma el control si no hay nadie o el actual tiene un rol igual o menor. Suelta lo que haya
    /// dejado el anterior y le avisa. Devuelve el guardián de la ranura (ya armado, sin ventana en la
    /// que un pánico la deje tomada) y el aviso de reemplazo de esta conexión.
    pub(super) fn tomar(self: &Arc<Self>, id: u64, rol: Rol) -> Option<(Dueno<S>, oneshot::Receiver<()>)> {
        let rx = self.con_ranura(|r| {
            if r.as_ref().is_some_and(|o| rol < o.rol) {
                return None;
            }
            if let Some(viejo) = r.take() {
                crate::info!("ctl: la conexión {id} reemplaza a la {}", viejo.id);
                let _ = viejo.reemplazo.send(());
            }
            let (tx, rx) = oneshot::channel();
            *r = Some(Ocupante { id, rol, reemplazo: tx });
            self.sesion.poner_mando(&EstadoMando::suelto());
            Some(rx)
        })?;
        Some((Dueno { e: self.clone(), id }, rx))
    }

    /// Pasa el estado a la PS5 solo si esta conexión es la que controla.
    pub(super) fn mandar(&self, id: u64, estado: &EstadoMando) {
        self.con_ranura(|r| {
            if r.as_ref().is_some_and(|o| o.id == id) {
                self.sesion.poner_mando(estado);
            }
        });
    }

    /// Suelta todo y libera la ranura, si esta conexión todavía la tiene.
    pub(super) fn soltar(&self, id: u64) {
        self.con_ranura(|r| {
            if r.as_ref().is_some_and(|o| o.id == id) {
                self.sesion.poner_mando(&EstadoMando::suelto());
                *r = None;
            }
        });
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
    Reemplazada,
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
    let Some((dueno, mut reemplazo)) = e.tomar(id, cred.rol) else {
        crate::info!("ctl: conexión {id} (rol={}) rechazada: hay un controlador de rol mayor", nombre_rol(cred.rol));
        return despedir(socket, Some(MsgServidor::error("OCUPADO"))).await;
    };
    // `exp` es hora de pared: se pasa una sola vez a un plazo del reloj monótono, con tope (un `exp`
    // enorme desbordaría el `Instant`).
    let vence = Instant::now() + Duration::from_secs(cred.vence_s().saturating_sub(ahora_s)).min(TOPE_CONEXION);
    if !enviar(&mut socket, &MsgServidor::Ok { rol: cred.rol }).await {
        return;
    }
    crate::info!("ctl: conexión {id} (rol={}) controla la PS5", nombre_rol(cred.rol));

    let mut filtro = Filtro::new(cred.rol);
    let fin = bucle(&e, id, &mut filtro, &mut socket, &mut reemplazo, vence).await;
    // Primero se suelta todo (si todavía controla) y recién después se habla con el cliente.
    drop(dueno);
    let descartados = filtro.descartados();
    e.descartados.fetch_add(descartados, Ordering::Relaxed);
    let (motivo, ultimo) = match fin {
        Fin::Cerrada => ("cerrada", None),
        Fin::Formato => ("mensaje inválido", Some(MsgServidor::error("FORMATO"))),
        Fin::Vencida => ("credencial vencida", Some(MsgServidor::error("VENCIDA"))),
        Fin::Reemplazada => ("reemplazada", Some(MsgServidor::Reemplazado)),
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
    reemplazo: &mut oneshot::Receiver<()>,
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
            _ = &mut *reemplazo => return Fin::Reemplazada,
            _ = sleep_until(vence) => return Fin::Vencida,
            _ = sleep_until(silencio.unwrap_or(vence)), if silencio.is_some() => {
                let ahora = e.ahora_ms();
                if guardian.vencido(ahora) {
                    crate::depurar!("ctl: conexión {id}: {SILENCIO_MS} ms sin mensajes; todo suelto");
                    let salida = filtro.aplicar(&EstadoMando::suelto(), ahora);
                    e.mandar(id, &salida);
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
                    e.mandar(id, &ultima_filtrada);
                }
                match m {
                    MsgCliente::Auth { .. } => return Fin::Formato,
                    MsgCliente::Estado { botones, l2, r2, lx, ly, rx, ry } => {
                        let entrada = EstadoMando { botones, l2, r2, lx, ly, rx, ry };
                        let salida = filtro.aplicar(&entrada, ahora);
                        ultima_filtrada = salida;
                        e.mandar(id, &salida);
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
