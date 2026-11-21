//! Registro a stderr con el prefijo de prioridad que entiende journald (`<6>`, `<4>`…).
//! El nivel de depuración se activa con `MITURNO_RP_DEPURAR=1`. Nunca se registran secretos ni el
//! nombre de la conexión de casa (si hace falta, solo en depuración).
//! Las macros nunca entran en pánico (a diferencia de `eprintln!` con stderr roto): un pánico al
//! registrar durante un cierre podría abortar el proceso sin cerrar la sesión con la PS5.
//! Los rechazos de pedidos que llegan de afuera usan `limitado!` (uno por minuto por tipo).

use std::sync::{Mutex, OnceLock};

#[doc(hidden)]
pub fn escribir(prioridad: u8, args: std::fmt::Arguments<'_>) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr(), "<{prioridad}>{args}");
}

pub fn depurando() -> bool {
    static D: OnceLock<bool> = OnceLock::new();
    *D.get_or_init(|| std::env::var("MITURNO_RP_DEPURAR").is_ok_and(|v| v == "1"))
}

/// Milisegundos de un reloj monótono desde el primer uso (para `Limitado`).
pub fn ms_desde_arranque() -> u64 {
    static T0: OnceLock<std::time::Instant> = OnceLock::new();
    T0.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64
}

/// Cada cuánto sale, como mucho, un aviso de cada tipo de rechazo (`/control/*` y `/ctl` son públicos
/// por Caddy: cualquiera puede llenar el journal).
pub const CADA_RECHAZO_MS: u64 = 60_000;

/// Limita un tipo de aviso a uno cada `cada_ms`; el que sale dice cuántos se callaron desde el anterior.
pub struct Limitado {
    /// (cuándo salió el último, cuántos se callaron desde entonces).
    estado: Mutex<Option<(u64, u64)>>,
}

impl Limitado {
    pub const fn new() -> Self {
        Self { estado: Mutex::new(None) }
    }

    /// `Some(callados)` si este aviso sale; `None` si se calla.
    pub fn permitir(&self, ahora_ms: u64, cada_ms: u64) -> Option<u64> {
        let mut e = self.estado.lock().unwrap_or_else(|e| e.into_inner());
        match *e {
            Some((ultimo, callados)) if ahora_ms.saturating_sub(ultimo) < cada_ms => {
                *e = Some((ultimo, callados + 1));
                None
            }
            Some((_, callados)) => {
                *e = Some((ahora_ms, 0));
                Some(callados)
            }
            None => {
                *e = Some((ahora_ms, 0));
                Some(0)
            }
        }
    }
}

impl Default for Limitado {
    fn default() -> Self {
        Self::new()
    }
}

/// Como `aviso!`/`info!`, pero como mucho uno cada `CADA_RECHAZO_MS` por `Limitado` (un `static`
/// por tipo de rechazo). El que sale agrega cuántos se callaron.
#[macro_export]
macro_rules! limitado {
    ($nivel:literal, $lim:expr, $($t:tt)*) => {
        if let Some(callados) = $lim.permitir($crate::registro::ms_desde_arranque(), $crate::registro::CADA_RECHAZO_MS) {
            if callados == 0 {
                $crate::registro::escribir($nivel, format_args!($($t)*))
            } else {
                $crate::registro::escribir(
                    $nivel,
                    format_args!("{} (y {callados} más en el último minuto)", format_args!($($t)*)),
                )
            }
        }
    };
}

#[macro_export]
macro_rules! info {
    ($($t:tt)*) => { $crate::registro::escribir(6, format_args!($($t)*)) };
}

#[macro_export]
macro_rules! aviso {
    ($($t:tt)*) => { $crate::registro::escribir(4, format_args!($($t)*)) };
}

#[macro_export]
macro_rules! falla {
    ($($t:tt)*) => { $crate::registro::escribir(3, format_args!($($t)*)) };
}

#[macro_export]
macro_rules! depurar {
    ($($t:tt)*) => { if $crate::registro::depurando() { $crate::registro::escribir(7, format_args!($($t)*)) } };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn el_primero_sale_y_los_siguientes_se_callan_hasta_que_pasa_el_intervalo() {
        let l = Limitado::new();
        assert_eq!(l.permitir(0, 60_000), Some(0), "el primero sale");
        assert_eq!(l.permitir(10, 60_000), None);
        assert_eq!(l.permitir(59_999, 60_000), None);
        assert_eq!(l.permitir(60_000, 60_000), Some(2), "sale con la cuenta de los que se callaron");
        assert_eq!(l.permitir(60_001, 60_000), None);
        assert_eq!(l.permitir(200_000, 60_000), Some(1));
        assert_eq!(l.permitir(260_000, 60_000), Some(0));
    }

    #[test]
    fn cada_limitador_cuenta_aparte() {
        let (a, b) = (Limitado::new(), Limitado::new());
        assert_eq!(a.permitir(0, 1_000), Some(0));
        assert_eq!(b.permitir(1, 1_000), Some(0), "otro tipo de aviso no se calla por el primero");
        assert_eq!(a.permitir(2, 1_000), None);
    }

    #[test]
    fn el_reloj_del_registro_no_retrocede() {
        let a = ms_desde_arranque();
        let b = ms_desde_arranque();
        assert!(b >= a);
    }
}
