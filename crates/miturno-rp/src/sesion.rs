//! La sesión con la PS5, detrás de un trait para poder probar el ciclo y el servidor (Task 5) con una
//! sesión falsa. La real (`SesionLibchiaki`) corre libchiaki en un hilo propio; todos los métodos son
//! rápidos (mandan un comando) y se pueden llamar desde cualquier tarea a través de un `Arc`.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rp_core::{EstadoMando, LimitadorKeyframe};
use tokio::sync::mpsc;

use crate::config::Perfil;

#[cfg(test)]
pub mod falsa;
pub mod libchiaki;

/// Lo que sale de la sesión, en orden, por el canal de `eventos()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventoSesion {
    Conectada,
    /// Un cuadro H.264 en Annex B, tal como llega de la PS5. `perdidos`: cuadros perdidos antes de este.
    Video { datos: Vec<u8>, perdidos: u32 },
    /// Un paquete Opus (vacío = paquete perdido; la línea de tiempo avanza igual).
    Audio { datos: Vec<u8> },
    FormatoAudio { canales: u8, frecuencia: u32, muestras_por_paquete: u32 },
    /// La sesión terminó sin que se la detuviera (una sesión detenida con `detener()` no avisa).
    Cerrada { motivo: MotivoCierre },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MotivoCierre {
    /// La PS5 respondió "Remote is already in use" (`0x80108b10`): queda una sesión vieja abierta.
    EnUso,
    /// No se pudo conectar o se cortó la conexión.
    Red,
    /// La PS5 cerró la sesión ella misma ("Server shutting down"; un corte de red no lo puede mandar).
    /// Medido con la consola real: pasa cuando alguien prende el mando físico y elige perfil. También
    /// lo producen reposo, red desactivada, actualización o apagado desde la consola. El ciclo no la
    /// retoma solo (ni después de un reinicio: `crate::pausa`): espera `/control/reconectar`.
    TomadaLocal,
    /// Lo que no se arregla solo: versión de Remote Play, Remote Play caído en la consola, PIN pedido,
    /// falla del control o no se pudo crear la sesión en libchiaki. Se reintenta más espaciado.
    Otro,
}

impl MotivoCierre {
    /// Para la espera hasta el próximo intento (`rp_core::reintento`).
    pub fn causa(self) -> rp_core::Causa {
        match self {
            MotivoCierre::EnUso => rp_core::Causa::EnUso,
            MotivoCierre::Red => rp_core::Causa::Red,
            // El ciclo no reintenta solo con TomadaLocal; si alguna vez lo hiciera, con la espera larga.
            MotivoCierre::Otro | MotivoCierre::TomadaLocal => rp_core::Causa::Otra,
        }
    }
}

/// Mínimo entre pedidos de keyframe (spec §5.2): pedirlos seguido congela la imagen.
pub const MIN_ENTRE_KEYFRAMES_MS: u64 = 3_000;

/// Código de aplicación de Remote Play con el que la PS5 rechaza una sesión nueva si hay otra abierta.
pub const CODIGO_EN_USO: u32 = 0x8010_8b10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorSesion {
    /// El hilo de la sesión terminó (no debería pasar mientras corre el proceso).
    HiloTerminado,
}

impl std::fmt::Display for ErrorSesion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ErrorSesion::HiloTerminado => f.write_str("el hilo de la sesión terminó"),
        }
    }
}

impl std::error::Error for ErrorSesion {}

pub trait SesionPs5: Send + Sync + 'static {
    /// Manda el paquete de encendido a la PS5 (si está en reposo, arranca).
    fn despertar(&self, perfil: &Perfil) -> Result<(), ErrorSesion>;
    /// Abre una sesión. El resultado llega por `eventos()`: `Conectada` o `Cerrada`.
    fn conectar(&self, perfil: &Perfil) -> Result<(), ErrorSesion>;
    /// Cierra la sesión actual con `session.stop()` y espera a que termine. Si no hay sesión, no hace nada.
    /// Obligatorio antes de salir: si no, la PS5 rechaza la próxima sesión con "Remote is already in use".
    fn detener(&self) -> impl Future<Output = ()> + Send + 'static;
    /// Pasa el estado del mando a la PS5 (ya filtrado por rol). Sin sesión, se descarta.
    fn poner_mando(&self, estado: &EstadoMando);
    /// Pide un keyframe a la PS5. Devuelve si el pedido salió (ver `ConLimite`).
    fn pedir_keyframe(&self) -> bool;
    /// Pone la PS5 en reposo (si hay sesión).
    fn reposo(&self);
    /// El canal de eventos. Hay uno solo: la primera llamada lo entrega y las siguientes devuelven `None`.
    fn eventos(&self) -> Option<mpsc::Receiver<EventoSesion>>;
}

/// Envuelve una sesión para que todos los pedidos de keyframe pasen por un único limitador
/// (mínimo `min_ms` entre pedidos). El ciclo y el servidor (Task 5) comparten este mismo envoltorio.
///
/// Un pedido que cae dentro de la ventana no se pierde: se difiere al final de la ventana (la PS5 no
/// manda keyframes sola, así que un lector que llega tarde se quedaría sin imagen hasta el próximo
/// pedido). Todos los rechazados de una misma ventana se juntan en **un solo** pedido diferido, que
/// pasa por el mismo limitador: si para entonces otro pedido ya ocupó el lugar, no se repite.
pub struct ConLimite<S> {
    c: Arc<Compartido<S>>,
}

struct Compartido<S> {
    interna: S,
    limitador: Mutex<LimitadorKeyframe>,
    /// Hay un pedido diferido esperando el final de la ventana.
    diferido: AtomicBool,
    t0: tokio::time::Instant,
}

impl<S: SesionPs5> Compartido<S> {
    /// Reloj monótono (Instant de tokio): el limitador asume que no retrocede.
    fn ahora_ms(&self) -> u64 {
        self.t0.elapsed().as_millis() as u64
    }

    /// Pasa por el limitador; si hay lugar, pide el keyframe. Si no, devuelve desde cuándo hay lugar.
    fn intentar(&self) -> Result<bool, u64> {
        let mut l = self.limitador.lock().unwrap_or_else(|e| e.into_inner());
        if l.permitir(self.ahora_ms()) {
            drop(l);
            Ok(self.interna.pedir_keyframe())
        } else {
            Err(l.libre_desde())
        }
    }
}

impl<S: SesionPs5> ConLimite<S> {
    pub fn new(interna: S, min_ms: u64) -> Self {
        Self {
            c: Arc::new(Compartido {
                interna,
                limitador: Mutex::new(LimitadorKeyframe::new(min_ms)),
                diferido: AtomicBool::new(false),
                t0: tokio::time::Instant::now(),
            }),
        }
    }

    /// Solo para leer el estado de la sesión: los keyframes siempre se piden con `pedir_keyframe`
    /// (si no, se saltea el limitador).
    pub fn interna(&self) -> &S {
        &self.c.interna
    }

    /// Agenda el pedido diferido (uno solo por ventana). La tarea guarda una referencia débil: no
    /// mantiene viva la sesión (su `Drop` cierra con `stop()`). Fuera de un runtime de tokio no se difiere.
    fn diferir(&self, libre_desde_ms: u64) {
        if self.c.diferido.swap(true, Ordering::AcqRel) {
            return; // ya hay uno esperando: este pedido queda cubierto por ese
        }
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            self.c.diferido.store(false, Ordering::Release);
            return;
        };
        let debil = Arc::downgrade(&self.c);
        let cuando = self.c.t0 + Duration::from_millis(libre_desde_ms);
        rt.spawn(async move {
            tokio::time::sleep_until(cuando).await;
            let Some(c) = debil.upgrade() else { return };
            c.diferido.store(false, Ordering::Release);
            // Si otro pedido ya ocupó el lugar (salió después de todos los que esperaban), no se repite.
            if let Ok(enviado) = c.intentar() {
                crate::depurar!("keyframe: pedido diferido {}", if enviado { "enviado" } else { "sin sesión" });
            }
        });
    }
}

impl<S: SesionPs5> SesionPs5 for ConLimite<S> {
    fn despertar(&self, perfil: &Perfil) -> Result<(), ErrorSesion> {
        self.c.interna.despertar(perfil)
    }
    fn conectar(&self, perfil: &Perfil) -> Result<(), ErrorSesion> {
        self.c.interna.conectar(perfil)
    }
    fn detener(&self) -> impl Future<Output = ()> + Send + 'static {
        self.c.interna.detener()
    }
    fn poner_mando(&self, estado: &EstadoMando) {
        self.c.interna.poner_mando(estado)
    }
    /// `true` si el pedido salió ya; `false` si cayó dentro de la ventana (queda diferido al final
    /// de la ventana, ver `ConLimite`) o si no hay sesión.
    fn pedir_keyframe(&self) -> bool {
        match self.c.intentar() {
            Ok(enviado) => enviado,
            Err(libre_desde_ms) => {
                self.diferir(libre_desde_ms);
                false
            }
        }
    }
    fn reposo(&self) {
        self.c.interna.reposo()
    }
    fn eventos(&self) -> Option<mpsc::Receiver<EventoSesion>> {
        self.c.interna.eventos()
    }
}

#[cfg(test)]
mod tests {
    use super::falsa::SesionFalsa;
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn con_limite_deja_pasar_un_keyframe_cada_3_s() {
        let s = ConLimite::new(SesionFalsa::new(), 3_000);
        assert!(s.pedir_keyframe());
        assert!(!s.pedir_keyframe());
        tokio::time::advance(Duration::from_millis(2_999)).await;
        assert!(!s.pedir_keyframe());
        assert_eq!(s.interna().registro().keyframes, 1);
        // A los 3 s sale el pedido diferido (uno solo por los dos rechazados) y ocupa el lugar.
        avanzar(1).await;
        assert_eq!(s.interna().registro().keyframes, 2);
        assert!(!s.pedir_keyframe(), "a 0 s del diferido no sale: queda otro diferido para t = 6");
        avanzar(3_000).await;
        assert_eq!(s.interna().registro().keyframes, 3);
    }

    /// Deja correr las tareas listas (el pedido diferido es una tarea de tokio).
    async fn correr_tareas() {
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
    }

    async fn avanzar(ms: u64) {
        tokio::time::advance(Duration::from_millis(ms)).await;
        correr_tareas().await;
    }

    #[tokio::test(start_paused = true)]
    async fn un_pedido_rechazado_sale_al_final_de_la_ventana() {
        let s = ConLimite::new(SesionFalsa::new(), 3_000);
        assert!(s.pedir_keyframe());
        avanzar(1_000).await;
        assert!(!s.pedir_keyframe(), "dentro de la ventana no sale ya");
        avanzar(1_999).await; // t = 2,999 s
        assert_eq!(s.interna().registro().keyframes, 1);
        avanzar(1).await; // t = 3 s
        assert_eq!(s.interna().registro().keyframes, 2, "el pedido diferido sale a los 3 s");
        avanzar(10_000).await;
        assert_eq!(s.interna().registro().keyframes, 2, "y uno solo");
    }

    #[tokio::test(start_paused = true)]
    async fn varios_rechazos_en_una_ventana_dan_un_solo_pedido_diferido() {
        let s = ConLimite::new(SesionFalsa::new(), 3_000);
        assert!(s.pedir_keyframe());
        for _ in 0..5 {
            avanzar(400).await;
            assert!(!s.pedir_keyframe());
        }
        avanzar(10_000).await;
        assert_eq!(s.interna().registro().keyframes, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn el_pedido_diferido_tambien_respeta_los_3_s() {
        let s = ConLimite::new(SesionFalsa::new(), 3_000);
        assert!(s.pedir_keyframe()); // t = 0
        avanzar(1_000).await;
        assert!(!s.pedir_keyframe()); // diferido a t = 3
        avanzar(2_000).await; // t = 3: sale el diferido y corre la ventana
        assert_eq!(s.interna().registro().keyframes, 2);
        avanzar(500).await; // t = 3,5
        assert!(!s.pedir_keyframe(), "a 0,5 s del diferido no sale");
        avanzar(2_499).await; // t = 5,999
        assert_eq!(s.interna().registro().keyframes, 2);
        avanzar(1).await; // t = 6
        assert_eq!(s.interna().registro().keyframes, 3, "el segundo diferido sale a 3 s del primero");
    }

    #[test]
    fn sin_runtime_de_tokio_un_rechazo_no_entra_en_panico() {
        let s = ConLimite::new(SesionFalsa::new(), 3_000);
        assert!(s.pedir_keyframe());
        assert!(!s.pedir_keyframe());
    }

    #[test]
    fn eventos_se_entrega_una_sola_vez() {
        let s = SesionFalsa::new();
        assert!(s.eventos().is_some());
        assert!(s.eventos().is_none());
    }
}
