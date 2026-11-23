//! `GET /control/mando`: el resumen del mando del jugador en vivo, por SSE (`text/event-stream`).
//!
//! - Solo con el token de control (la misma capa que `/control/salud`).
//! - El primer evento es el resumen actual; después, uno por cada cambio (`ResumenMando`, que ya
//!   publica solo lo que cambió). Los cambios que llegan mientras corre el piso se funden en el último
//!   valor (`watch`), y ese último siempre sale.
//! - Piso de `MANDO_PISO_MS` entre dos envíos al mismo suscriptor, medido desde el envío anterior (con
//!   el reloj de tokio). Un valor igual al ya enviado (A, B, A dentro del piso) no se repite.
//! - Keep-alive: un comentario SSE cada 15 s.
//! - Como mucho `MANDO_SUSCRIPTORES_MAX` a la vez; el siguiente recibe 503 `MANDO_OCUPADO`. El lugar lo
//!   devuelve un guardián al soltarse el stream (cierre del cliente, pánico o tarea cancelada).

use std::convert::Infallible;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::stream;
use rp_core::ResumenMando;
use tokio::sync::watch;
use tokio::time::{sleep_until, Instant};

use super::{codigo, Compartido};
use crate::sesion::SesionPs5;

/// Tiempo mínimo entre dos envíos al mismo suscriptor (≤ 15 por segundo).
pub const MANDO_PISO_MS: u64 = 66;
/// Suscriptores a la vez.
pub const MANDO_SUSCRIPTORES_MAX: usize = 4;
const KEEP_ALIVE: Duration = Duration::from_secs(15);

/// Cuántos suscriptores hay ahora.
#[derive(Default)]
pub(super) struct Suscriptores(AtomicUsize);

/// Un lugar de `Suscriptores`: se devuelve al soltarlo.
struct Lugar(Arc<Suscriptores>);

impl Suscriptores {
    fn tomar(self: &Arc<Self>) -> Option<Lugar> {
        self.0.try_update(Ordering::AcqRel, Ordering::Acquire, |n| (n < MANDO_SUSCRIPTORES_MAX).then_some(n + 1)).ok()?;
        Some(Lugar(self.clone()))
    }
}

impl Drop for Lugar {
    fn drop(&mut self) {
        self.0 .0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Lo que lleva el stream de un suscriptor.
struct Flujo {
    rx: watch::Receiver<ResumenMando>,
    /// Lo último que se mandó y cuándo; `None` hasta el primer evento.
    enviado: Option<(ResumenMando, Instant)>,
    _lugar: Lugar,
}

impl Flujo {
    /// El próximo evento a mandar, o `None` si el difusor ya no existe (el servidor se apaga).
    async fn siguiente(&mut self) -> Option<Event> {
        let valor = loop {
            let Some((previo, cuando)) = self.enviado.as_ref() else {
                break self.rx.borrow_and_update().clone();
            };
            self.rx.changed().await.ok()?;
            sleep_until(*cuando + Duration::from_millis(MANDO_PISO_MS)).await;
            let v = self.rx.borrow_and_update().clone();
            if v != *previo {
                break v;
            }
        };
        let evento = Event::default().data(serde_json::to_string(&valor).ok()?);
        self.enviado = Some((valor, Instant::now()));
        Some(evento)
    }
}

pub(super) async fn mando<S: SesionPs5>(State(e): State<Compartido<S>>) -> Response {
    let Some(lugar) = e.suscriptores_mando.tomar() else {
        static LIM: crate::registro::Limitado = crate::registro::Limitado::new();
        crate::limitado!(4, LIM, "control/mando: demasiados suscriptores; rechazado");
        return codigo(StatusCode::SERVICE_UNAVAILABLE, "MANDO_OCUPADO");
    };
    let flujo = Flujo { rx: e.mando_tx.subscribe(), enviado: None, _lugar: lugar };
    let eventos = stream::unfold(flujo, |mut f| async move {
        let ev = f.siguiente().await?;
        Some((Ok::<_, Infallible>(ev), f))
    });
    Sse::new(eventos).keep_alive(KeepAlive::new().interval(KEEP_ALIVE).text("")).into_response()
}
