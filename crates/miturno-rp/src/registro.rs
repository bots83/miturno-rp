//! Registro a stderr con el prefijo de prioridad que entiende journald (`<6>`, `<4>`…).
//! El nivel de depuración se activa con `MITURNO_RP_DEPURAR=1`. Nunca se registran secretos ni el
//! nombre de la conexión de casa (si hace falta, solo en depuración).

use std::sync::OnceLock;

pub fn depurando() -> bool {
    static D: OnceLock<bool> = OnceLock::new();
    *D.get_or_init(|| std::env::var("MITURNO_RP_DEPURAR").is_ok_and(|v| v == "1"))
}

#[macro_export]
macro_rules! info {
    ($($t:tt)*) => { eprintln!("<6>{}", format_args!($($t)*)) };
}

#[macro_export]
macro_rules! aviso {
    ($($t:tt)*) => { eprintln!("<4>{}", format_args!($($t)*)) };
}

#[macro_export]
macro_rules! falla {
    ($($t:tt)*) => { eprintln!("<3>{}", format_args!($($t)*)) };
}

#[macro_export]
macro_rules! depurar {
    ($($t:tt)*) => { if $crate::registro::depurando() { eprintln!("<7>{}", format_args!($($t)*)) } };
}
