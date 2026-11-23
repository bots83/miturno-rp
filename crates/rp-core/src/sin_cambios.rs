//! Cuánto hace que el jugador del turno vigente no cambia nada de su mando (`jugador.sinCambiosMs`).
//!
//! A diferencia de `Actividad`, que alimenta el aviso y el corte por inactividad y por eso trata un
//! botón sostenido como actividad, acá solo cuenta un cambio real: un bit de botones, un gatillo o un
//! stick que cruza su umbral o, si el stick ya lo pasó, que se mueve más de `UMBRAL_STICK` respecto del
//! último valor contado. Sostener algo no reinicia la cuenta. El reloj lo inyecta quien llama.
//!
//! Solo se le registran estados reales que mandó el jugador: los sintéticos (el todo suelto por
//! silencio, su re-aplicación al volver, el soltar al cerrar) no pasan por acá.

use crate::actividad::{UMBRAL_GATILLO, UMBRAL_STICK};
use crate::mando::EstadoMando;

#[derive(Debug, Default)]
pub struct SinCambios {
    turno: Option<String>,
    ultima: u64,
    /// Último valor contado por botón, gatillo y eje (los gatillos, solo si pasaron el umbral).
    botones: u32,
    l2: bool,
    r2: bool,
    ejes: [i16; 4],
}

impl SinCambios {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fija el turno vigente. Si cambia, reinicia con `ultima = ahora`; si es el mismo, no hace nada.
    pub fn turno(&mut self, turn_id: Option<&str>, ahora_ms: u64) {
        if self.turno.as_deref() == turn_id {
            return;
        }
        *self = Self { turno: turn_id.map(String::from), ultima: ahora_ms, ..Self::default() };
    }

    /// Registra un estado real del jugador del turno `turn_id` (otro turno se ignora).
    pub fn registrar(&mut self, turn_id: &str, estado: &EstadoMando, ahora_ms: u64) {
        if self.turno.as_deref() != Some(turn_id) {
            return;
        }
        let mut cambio = false;
        if estado.botones != self.botones {
            self.botones = estado.botones;
            cambio = true;
        }
        for (contado, v) in [(&mut self.l2, estado.l2), (&mut self.r2, estado.r2)] {
            let pasa = v > UMBRAL_GATILLO;
            if pasa != *contado {
                *contado = pasa;
                cambio = true;
            }
        }
        for (contado, v) in self.ejes.iter_mut().zip([estado.lx, estado.ly, estado.rx, estado.ry]) {
            let pasa = |x: i16| (x as i32).abs() > UMBRAL_STICK as i32;
            let salto = (v as i32 - *contado as i32).abs() > UMBRAL_STICK as i32;
            let cuenta = pasa(v) != pasa(*contado) || (pasa(*contado) && salto);
            if cuenta {
                *contado = v;
                cambio = true;
            }
        }
        if cambio {
            self.ultima = ahora_ms;
        }
    }

    /// Milisegundos desde el último cambio real; `None` sin turno vigente.
    pub fn ms(&self, ahora_ms: u64) -> Option<u64> {
        self.turno.as_ref()?;
        Some(ahora_ms.saturating_sub(self.ultima))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mando::{EstadoMando, CRUZ, L1};

    fn con(f: impl FnOnce(&mut EstadoMando)) -> EstadoMando {
        let mut e = EstadoMando::suelto();
        f(&mut e);
        e
    }

    fn nuevo(ahora: u64) -> SinCambios {
        let mut s = SinCambios::new();
        s.turno(Some("t1"), ahora);
        s
    }

    #[test]
    fn sin_turno_es_none() {
        assert_eq!(SinCambios::new().ms(5), None);
        let mut s = nuevo(0);
        s.turno(None, 10);
        assert_eq!(s.ms(20), None);
    }

    #[test]
    fn cambio_de_turno_reinicia_a_cero() {
        let mut s = nuevo(1000);
        assert_eq!(s.ms(4000), Some(3000));
        s.turno(Some("t1"), 3000);
        assert_eq!(s.ms(4000), Some(3000), "el mismo turno no reinicia");
        s.turno(Some("t2"), 5000);
        assert_eq!(s.ms(5000), Some(0));
        assert_eq!(s.ms(7000), Some(2000));
    }

    #[test]
    fn boton_sostenido_no_reinicia() {
        let mut s = nuevo(0);
        let e = con(|e| e.botones = CRUZ);
        s.registrar("t1", &e, 0);
        s.registrar("t1", &e, 5000);
        assert_eq!(s.ms(6000), Some(6000));
    }

    #[test]
    fn cambio_de_bit_reinicia() {
        let mut s = nuevo(0);
        s.registrar("t1", &con(|e| e.botones = CRUZ), 1000);
        assert_eq!(s.ms(1500), Some(500));
        s.registrar("t1", &con(|e| e.botones = CRUZ | L1), 2000);
        assert_eq!(s.ms(2500), Some(500));
        // Soltar desde la web también es un cambio real.
        s.registrar("t1", &EstadoMando::suelto(), 3000);
        assert_eq!(s.ms(3200), Some(200));
    }

    #[test]
    fn gatillo_cuenta_solo_al_cruzar_32() {
        let mut s = nuevo(0);
        let r2 = |v: u8| con(|e| e.r2 = v);
        s.registrar("t1", &r2(10), 100);
        s.registrar("t1", &r2(30), 200);
        assert_eq!(s.ms(300), Some(300), "10→30 no cuenta");
        s.registrar("t1", &r2(40), 400);
        assert_eq!(s.ms(500), Some(100), "30→40 cruza 32");
        s.registrar("t1", &r2(200), 600);
        assert_eq!(s.ms(700), Some(300), "40→200 no cuenta");
        s.registrar("t1", &r2(20), 800);
        assert_eq!(s.ms(900), Some(100), "200→20 cruza 32");
        // Lo mismo con L2.
        s.registrar("t1", &con(|e| e.l2 = 255), 1000);
        assert_eq!(s.ms(1100), Some(100));
    }

    #[test]
    fn stick_cruza_umbral() {
        let mut s = nuevo(0);
        s.registrar("t1", &con(|e| e.lx = 9000), 1000);
        assert_eq!(s.ms(1100), Some(100), "0→9000 cuenta");
        s.registrar("t1", &con(|e| e.lx = 0), 2000);
        assert_eq!(s.ms(2100), Some(100), "9000→0 cuenta");
        s.registrar("t1", &con(|e| e.ry = 6000), 3000);
        assert_eq!(s.ms(3100), Some(1100), "la deriva no cuenta");
    }

    #[test]
    fn stick_pasado_el_umbral_cuenta_saltos_de_mas_de_8192() {
        let mut s = nuevo(0);
        s.registrar("t1", &con(|e| e.ly = -20000), 1000);
        assert_eq!(s.ms(1000), Some(0));
        s.registrar("t1", &con(|e| e.ly = -10000), 2000);
        assert_eq!(s.ms(2000), Some(0), "-20000→-10000 cuenta");

        let mut s = nuevo(0);
        s.registrar("t1", &con(|e| e.ly = -20000), 1000);
        s.registrar("t1", &con(|e| e.ly = -18000), 2000);
        assert_eq!(s.ms(2000), Some(1000), "-20000→-18000 no cuenta");
        // El último contado no se movió (sigue en -20000): -11000 está a 9000 de él, aunque solo
        // a 7000 de lo último que llegó.
        s.registrar("t1", &con(|e| e.ly = -11000), 3000);
        assert_eq!(s.ms(3000), Some(0));
    }

    #[test]
    fn otro_turno_se_ignora() {
        let mut s = nuevo(1000);
        s.registrar("t0", &con(|e| e.botones = CRUZ), 2000);
        assert_eq!(s.ms(3000), Some(2000));
        let mut vacio = SinCambios::new();
        vacio.registrar("t1", &con(|e| e.botones = CRUZ), 10);
        assert_eq!(vacio.ms(20), None);
    }

    #[test]
    fn mismo_estado_tras_soltar_sintetico_no_cuenta() {
        let mut s = nuevo(0);
        let e = con(|e| e.botones = CRUZ);
        s.registrar("t1", &e, 1000);
        // El todo suelto por silencio no se registra; al volver, se reaplica el mismo estado.
        s.registrar("t1", &e, 9000);
        assert_eq!(s.ms(10_000), Some(9000));
    }
}
