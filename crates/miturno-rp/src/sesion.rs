//! La sesión con la PS5, detrás de un trait para poder probar el ciclo y el servidor (Task 5) con una
//! sesión falsa. La real (`SesionLibchiaki`) corre libchiaki en un hilo propio; todos los métodos son
//! rápidos (mandan un comando) y se pueden llamar desde cualquier tarea a través de un `Arc`.

use std::future::Future;
use std::sync::Mutex;

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
    Otro,
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
pub struct ConLimite<S> {
    interna: S,
    limitador: Mutex<LimitadorKeyframe>,
    t0: tokio::time::Instant,
}

impl<S: SesionPs5> ConLimite<S> {
    pub fn new(interna: S, min_ms: u64) -> Self {
        Self { interna, limitador: Mutex::new(LimitadorKeyframe::new(min_ms)), t0: tokio::time::Instant::now() }
    }

    pub fn interna(&self) -> &S {
        &self.interna
    }
}

impl<S: SesionPs5> SesionPs5 for ConLimite<S> {
    fn despertar(&self, perfil: &Perfil) -> Result<(), ErrorSesion> {
        self.interna.despertar(perfil)
    }
    fn conectar(&self, perfil: &Perfil) -> Result<(), ErrorSesion> {
        self.interna.conectar(perfil)
    }
    fn detener(&self) -> impl Future<Output = ()> + Send + 'static {
        self.interna.detener()
    }
    fn poner_mando(&self, estado: &EstadoMando) {
        self.interna.poner_mando(estado)
    }
    fn pedir_keyframe(&self) -> bool {
        // Reloj monótono (Instant de tokio): el limitador asume que no retrocede.
        let ahora_ms = self.t0.elapsed().as_millis() as u64;
        let permitido = self.limitador.lock().unwrap_or_else(|e| e.into_inner()).permitir(ahora_ms);
        permitido && self.interna.pedir_keyframe()
    }
    fn reposo(&self) {
        self.interna.reposo()
    }
    fn eventos(&self) -> Option<mpsc::Receiver<EventoSesion>> {
        self.interna.eventos()
    }
}

#[cfg(test)]
mod tests {
    use super::falsa::SesionFalsa;
    use super::*;
    use std::time::Duration;

    #[tokio::test(start_paused = true)]
    async fn con_limite_deja_pasar_un_keyframe_cada_3_s() {
        let s = ConLimite::new(SesionFalsa::new(), 3_000);
        assert!(s.pedir_keyframe());
        assert!(!s.pedir_keyframe());
        tokio::time::advance(Duration::from_millis(2_999)).await;
        assert!(!s.pedir_keyframe());
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(s.pedir_keyframe());
        assert_eq!(s.interna().registro().keyframes, 2);
    }

    #[test]
    fn eventos_se_entrega_una_sola_vez() {
        let s = SesionFalsa::new();
        assert!(s.eventos().is_some());
        assert!(s.eventos().is_none());
    }
}
