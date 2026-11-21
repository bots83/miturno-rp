//! `GET /ctl`: el WebSocket de los mandos.
//!
//! - El primer mensaje tiene que ser `auth` en menos de 2 s; si no, `error FORMATO` y se cierra.
//! - Un solo controlador a la vez: una conexión válida de rol igual o mayor reemplaza a la anterior
//!   (que recibe `reemplazado` y se cierra); una de rol menor recibe `error OCUPADO`.
//! - Cada mensaje válido es un latido. Con 500 ms de silencio se suelta todo (la conexión sigue); al
//!   cerrar, al llegar un mensaje inválido, al ser reemplazada o al vencer la credencial, también.
//! - El estado del mando pasa por el filtro del rol; si Options del jugador llega a su máximo apretado,
//!   se vuelve a aplicar la última entrada en ese instante aunque solo lleguen latidos.
//! - Los pedidos de keyframe pasan por el limitador único (`ConLimite`).
//!
//! Solo el controlador actual llega a `poner_mando`, y siempre con la ranura tomada: así el "todo
//! suelto" de una conexión vieja nunca pisa a la nueva.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{close_code, CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use rp_core::{leer_mensaje, validar_con, EstadoMando, Filtro, Guardian, MsgCliente, MsgServidor, Rol};
use tokio::sync::oneshot;
use tokio::time::{sleep_until, timeout, Instant};

use super::EstadoServidor;
use crate::sesion::SesionPs5;

/// Plazo para el mensaje `auth`.
pub const PLAZO_AUTH: Duration = Duration::from_secs(2);
/// Silencio (sin mensajes ni latidos) tras el que se suelta todo.
pub const SILENCIO_MS: u64 = 500;
/// Tope de un mensaje (una credencial ocupa menos de 1 KiB).
const MAX_MENSAJE: usize = 4 * 1024;
/// Plazo para mandar el último mensaje antes de cerrar (un cliente que no lee no traba la tarea).
const PLAZO_ENVIO: Duration = Duration::from_secs(1);

pub(super) struct Ocupante {
    id: u64,
    rol: Rol,
    reemplazo: oneshot::Sender<()>,
}

impl<S: SesionPs5> EstadoServidor<S> {
    fn ahora_ms(&self) -> u64 {
        self.t0.elapsed().as_millis() as u64
    }

    fn con_ranura<R>(&self, f: impl FnOnce(&mut Option<Ocupante>) -> R) -> R {
        f(&mut self.ranura.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Toma el control si no hay nadie o el actual tiene un rol igual o menor. Suelta lo que haya
    /// dejado el anterior y le avisa. Devuelve el aviso de reemplazo de esta conexión.
    pub(super) fn tomar(&self, id: u64, rol: Rol) -> Option<oneshot::Receiver<()>> {
        self.con_ranura(|r| {
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
        })
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

pub(super) async fn ctl<S: SesionPs5>(State(e): State<Arc<EstadoServidor<S>>>, ws: WebSocketUpgrade) -> Response {
    ws.max_message_size(MAX_MENSAJE).max_frame_size(MAX_MENSAJE).on_upgrade(move |socket| conexion(e, socket))
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

enum Fin {
    Cerrada,
    Formato,
    Vencida,
    Reemplazada,
}

async fn conexion<S: SesionPs5>(e: Arc<EstadoServidor<S>>, mut socket: WebSocket) {
    let id = e.proximo_id.fetch_add(1, Ordering::Relaxed);
    let token = match timeout(PLAZO_AUTH, leer(&mut socket)).await {
        Ok(Leido::Mensaje(MsgCliente::Auth { token })) => token,
        Ok(Leido::Cerrada) => return,
        _ => return despedir(socket, Some(MsgServidor::error("FORMATO"))).await,
    };
    let ahora_s = (e.reloj_s)();
    let cred = match validar_con(&token, &e.clave, ahora_s) {
        Ok(c) => c,
        Err(err) => {
            crate::info!("ctl: credencial rechazada ({})", err.codigo());
            return despedir(socket, Some(MsgServidor::error(err.codigo()))).await;
        }
    };
    drop(token);
    let Some(mut reemplazo) = e.tomar(id, cred.rol) else {
        crate::info!("ctl: conexión {id} ({:?}) rechazada: hay un controlador de rol mayor", cred.rol);
        return despedir(socket, Some(MsgServidor::error("OCUPADO"))).await;
    };
    // `exp` es hora de pared: se pasa una sola vez a un plazo del reloj monótono.
    let vence = Instant::now() + Duration::from_secs(cred.vence_s().saturating_sub(ahora_s));
    if !enviar(&mut socket, &MsgServidor::Ok { rol: cred.rol }).await {
        e.soltar(id);
        return;
    }
    crate::info!("ctl: conexión {id} ({:?}) controla la PS5", cred.rol);

    let fin = bucle(&e, id, Filtro::new(cred.rol, e.limites), &mut socket, &mut reemplazo, vence).await;
    // Primero se suelta todo (si todavía controla) y recién después se habla con el cliente.
    e.soltar(id);
    let (motivo, ultimo) = match fin {
        Fin::Cerrada => ("cerrada", None),
        Fin::Formato => ("mensaje inválido", Some(MsgServidor::error("FORMATO"))),
        Fin::Vencida => ("credencial vencida", Some(MsgServidor::error("VENCIDA"))),
        Fin::Reemplazada => ("reemplazada", Some(MsgServidor::Reemplazado)),
    };
    crate::info!("ctl: conexión {id} terminó ({motivo})");
    if ultimo.is_some() {
        despedir(socket, ultimo).await;
    }
}

async fn bucle<S: SesionPs5>(
    e: &EstadoServidor<S>,
    id: u64,
    mut filtro: Filtro,
    socket: &mut WebSocket,
    reemplazo: &mut oneshot::Receiver<()>,
    vence: Instant,
) -> Fin {
    let mut guardian = Guardian::new(SILENCIO_MS);
    let mut entrada = EstadoMando::suelto();
    let mut ultimo = e.ahora_ms();
    guardian.latido(ultimo);
    let mut suelto_por_silencio = false;
    let ms = |m: u64| e.t0 + Duration::from_millis(m);

    loop {
        let silencio = (!suelto_por_silencio).then(|| ms(ultimo + SILENCIO_MS));
        let options = filtro.vence_en().map(ms);
        tokio::select! {
            biased;
            _ = &mut *reemplazo => return Fin::Reemplazada,
            _ = sleep_until(vence) => return Fin::Vencida,
            _ = sleep_until(silencio.unwrap_or(vence)), if silencio.is_some() => {
                let ahora = e.ahora_ms();
                if guardian.vencido(ahora) {
                    crate::depurar!("ctl: conexión {id}: {SILENCIO_MS} ms sin mensajes; todo suelto");
                    entrada = EstadoMando::suelto();
                    let salida = filtro.aplicar(&entrada, ahora);
                    e.mandar(id, &salida);
                    suelto_por_silencio = true;
                }
            }
            // Options del jugador llegó al máximo apretado: se vuelve a filtrar la última entrada.
            _ = sleep_until(options.unwrap_or(vence)), if options.is_some() => {
                let salida = filtro.aplicar(&entrada, e.ahora_ms());
                e.mandar(id, &salida);
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
                suelto_por_silencio = false;
                match m {
                    MsgCliente::Auth { .. } => return Fin::Formato,
                    MsgCliente::Estado { botones, l2, r2, lx, ly, rx, ry } => {
                        entrada = EstadoMando { botones, l2, r2, lx, ly, rx, ry };
                        let salida = filtro.aplicar(&entrada, ahora);
                        e.mandar(id, &salida);
                    }
                    MsgCliente::Latido {} => {}
                    MsgCliente::PedirKeyframe {} => {
                        let enviado = e.sesion.pedir_keyframe();
                        crate::depurar!("ctl: conexión {id}: keyframe {}", if enviado { "pedido" } else { "limitado" });
                    }
                }
            }
        }
    }
}
