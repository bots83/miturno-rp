//! Medición de la actividad del jugador (para el aviso y el corte por inactividad).

use crate::mando::EstadoMando;

/// Un eje de stick cuenta como actividad solo si pasa de este valor (la deriva queda abajo).
pub const UMBRAL_STICK: i16 = 8192;
/// Un gatillo cuenta como actividad solo si pasa de este valor.
pub const UMBRAL_GATILLO: u8 = 32;

/// ¿Este estado del mando es una acción deliberada del jugador?
pub fn es_actividad(e: &EstadoMando) -> bool {
    let eje = |v: i16| (v as i32).abs() > UMBRAL_STICK as i32;
    e.botones != 0 || e.l2 > UMBRAL_GATILLO || e.r2 > UMBRAL_GATILLO || eje(e.lx) || eje(e.ly) || eje(e.rx) || eje(e.ry)
}

/// Mide cuánto hace que el jugador del turno vigente no hace nada. El reloj lo inyecta quien llama.
#[derive(Debug, Default)]
pub struct Actividad {
    turno: Option<String>,
    ultima: u64,
    activo: bool,
}

impl Actividad {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fija el turno vigente. Si cambia, reinicia con `ultima = ahora`; si es el mismo, no hace nada.
    pub fn turno(&mut self, turn_id: Option<&str>, ahora_ms: u64) {
        if self.turno.as_deref() == turn_id {
            return;
        }
        self.turno = turn_id.map(String::from);
        self.ultima = ahora_ms;
        self.activo = false;
    }

    /// Registra el último estado del mando del turno `turn_id` (otro turno se ignora).
    pub fn registrar(&mut self, turn_id: &str, salida: &EstadoMando, ahora_ms: u64) {
        if self.turno.as_deref() != Some(turn_id) {
            return;
        }
        if es_actividad(salida) {
            self.activo = true;
            self.ultima = ahora_ms;
        } else {
            self.soltar(turn_id, ahora_ms);
        }
    }

    /// Cierre o silencio de la conexión: lo que estaba sostenido deja de contar desde ahora.
    pub fn soltar(&mut self, turn_id: &str, ahora_ms: u64) {
        if self.turno.as_deref() == Some(turn_id) && self.activo {
            self.activo = false;
            self.ultima = ahora_ms;
        }
    }

    /// Milisegundos sin actividad; `None` sin turno; `0` mientras haya actividad sostenida.
    pub fn inactivo_ms(&self, ahora_ms: u64) -> Option<u64> {
        self.turno.as_ref()?;
        Some(if self.activo { 0 } else { ahora_ms.saturating_sub(self.ultima) })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::mando::{EstadoMando, CRUZ};

    #[test]
    fn es_actividad_respeta_umbrales() {
        assert!(!es_actividad(&EstadoMando::suelto()));
        assert!(es_actividad(&EstadoMando { botones: CRUZ, ..Default::default() }));
        assert!(!es_actividad(&EstadoMando { r2: 32, ..Default::default() }));
        assert!(es_actividad(&EstadoMando { r2: 33, ..Default::default() }));
        assert!(!es_actividad(&EstadoMando { l2: 32, ..Default::default() }));
        assert!(es_actividad(&EstadoMando { l2: 33, ..Default::default() }));
        assert!(!es_actividad(&EstadoMando { lx: 8192, ..Default::default() }));
        assert!(es_actividad(&EstadoMando { lx: -8193, ..Default::default() }));
        assert!(es_actividad(&EstadoMando { ry: 9000, ..Default::default() }));
    }

    #[test]
    fn sin_turno_no_hay_medida() {
        assert_eq!(Actividad::new().inactivo_ms(5), None);
    }

    #[test]
    fn solo_turno_cuenta_desde_el_inicio() {
        let mut a = Actividad::new();
        a.turno(Some("t1"), 1000);
        assert_eq!(a.inactivo_ms(91_000), Some(90_000));
    }

    #[test]
    fn actividad_sostenida_da_cero_hasta_soltar() {
        let mut a = Actividad::new();
        a.turno(Some("t1"), 0);
        a.registrar("t1", &EstadoMando { r2: 255, ..Default::default() }, 1000);
        assert_eq!(a.inactivo_ms(200_000), Some(0));
        a.soltar("t1", 200_000);
        assert_eq!(a.inactivo_ms(290_000), Some(90_000));
    }

    #[test]
    fn la_deriva_no_cuenta() {
        let mut a = Actividad::new();
        a.turno(Some("t1"), 0);
        let e = EstadoMando { lx: 6000, ..Default::default() };
        let mut t = 0;
        while t <= 120_000 {
            a.registrar("t1", &e, t);
            t += 200;
        }
        assert_eq!(a.inactivo_ms(120_000), Some(120_000));
    }

    #[test]
    fn soltar_de_otro_turno_se_ignora() {
        let mut a = Actividad::new();
        a.turno(Some("t1"), 0);
        a.registrar("t1", &EstadoMando { botones: CRUZ, ..Default::default() }, 1000);
        a.soltar("t0", 5000);
        assert_eq!(a.inactivo_ms(9000), Some(0), "sigue sostenido: soltar de otro turno no cuenta");
        a.soltar("t1", 9000);
        assert_eq!(a.inactivo_ms(10_000), Some(1000));
        // Soltar dos veces no corre el inicio de la cuenta.
        a.soltar("t1", 9500);
        assert_eq!(a.inactivo_ms(10_000), Some(1000));
        // Sin turno, soltar no hace nada.
        let mut b = Actividad::new();
        b.soltar("t1", 10);
        assert_eq!(b.inactivo_ms(20), None);
    }

    #[test]
    fn otro_turno_se_ignora_y_el_mismo_no_reinicia() {
        let mut a = Actividad::new();
        a.turno(Some("t1"), 1000);
        a.registrar("t0", &EstadoMando { botones: CRUZ, ..Default::default() }, 2000);
        assert_eq!(a.inactivo_ms(3000), Some(2000));
        a.turno(Some("t1"), 2500);
        assert_eq!(a.inactivo_ms(3000), Some(2000));
        a.turno(Some("t2"), 5000);
        assert_eq!(a.inactivo_ms(6000), Some(1000));
        a.turno(None, 7000);
        assert_eq!(a.inactivo_ms(8000), None);
    }
}
