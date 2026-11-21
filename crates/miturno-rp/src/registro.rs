//! Registro a stderr con el prefijo de prioridad que entiende journald (`<6>`, `<4>`…).
//! El nivel de depuración se activa con `MITURNO_RP_DEPURAR=1`. Nunca se registran secretos ni el
//! nombre de la conexión de casa (si hace falta, solo en depuración).
//! Las macros nunca entran en pánico (a diferencia de `eprintln!` con stderr roto): un pánico al
//! registrar durante un cierre podría abortar el proceso sin cerrar la sesión con la PS5.

use std::sync::OnceLock;

#[doc(hidden)]
pub fn escribir(prioridad: u8, args: std::fmt::Arguments<'_>) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr(), "<{prioridad}>{args}");
}

pub fn depurando() -> bool {
    static D: OnceLock<bool> = OnceLock::new();
    *D.get_or_init(|| std::env::var("MITURNO_RP_DEPURAR").is_ok_and(|v| v == "1"))
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
