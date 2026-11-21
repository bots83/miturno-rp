//! Sesión falsa para los tests (del ciclo y del servidor de la Task 5): anota cada llamada con la hora
//! (reloj de tokio, que en los tests está pausado) y responde a `conectar` según un guion.

use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Arc, Mutex};

use rp_core::EstadoMando;
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::{ErrorSesion, EventoSesion, SesionPs5};
use crate::config::Perfil;

#[derive(Debug, Default, Clone)]
pub struct Registro {
    pub despertar: Vec<Instant>,
    pub conectar: Vec<Instant>,
    pub detener: usize,
    pub keyframes: usize,
    pub mandos: Vec<EstadoMando>,
    pub reposo: usize,
}

pub struct SesionFalsa {
    registro: Arc<Mutex<Registro>>,
    /// Qué eventos manda cada `conectar`, en orden. Si se acaba, `conectar` no responde nada.
    guion: Mutex<VecDeque<Vec<EventoSesion>>>,
    tx: mpsc::Sender<EventoSesion>,
    rx: Mutex<Option<mpsc::Receiver<EventoSesion>>>,
}

impl SesionFalsa {
    pub fn new() -> Self {
        Self::con_guion(Vec::new())
    }

    pub fn con_guion(guion: Vec<Vec<EventoSesion>>) -> Self {
        let (tx, rx) = mpsc::channel(4096);
        Self {
            registro: Arc::default(),
            guion: Mutex::new(guion.into()),
            tx,
            rx: Mutex::new(Some(rx)),
        }
    }

    pub fn registro(&self) -> Registro {
        self.registro.lock().unwrap().clone()
    }

    /// Para que el test mande eventos cuando quiera (por ejemplo, cuadros durante la transmisión).
    pub fn emisor(&self) -> mpsc::Sender<EventoSesion> {
        self.tx.clone()
    }
}

impl SesionPs5 for SesionFalsa {
    fn despertar(&self, _perfil: &Perfil) -> Result<(), ErrorSesion> {
        self.registro.lock().unwrap().despertar.push(Instant::now());
        Ok(())
    }

    fn conectar(&self, _perfil: &Perfil) -> Result<(), ErrorSesion> {
        self.registro.lock().unwrap().conectar.push(Instant::now());
        if let Some(eventos) = self.guion.lock().unwrap().pop_front() {
            for e in eventos {
                self.tx.try_send(e).expect("canal de la sesión falsa lleno");
            }
        }
        Ok(())
    }

    fn detener(&self) -> impl Future<Output = ()> + Send + 'static {
        self.registro.lock().unwrap().detener += 1;
        std::future::ready(())
    }

    fn poner_mando(&self, estado: &EstadoMando) {
        self.registro.lock().unwrap().mandos.push(*estado);
    }

    fn pedir_keyframe(&self) -> bool {
        self.registro.lock().unwrap().keyframes += 1;
        true
    }

    fn reposo(&self) {
        self.registro.lock().unwrap().reposo += 1;
    }

    fn eventos(&self) -> Option<mpsc::Receiver<EventoSesion>> {
        self.rx.lock().unwrap().take()
    }
}
