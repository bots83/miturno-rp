//! Lógica pura de miturno-rp: no depende de libchiaki ni de la red, así se prueba en cualquier máquina.

pub mod credencial;
pub mod filtro;
pub mod guardian;
pub mod limitador;
pub mod mando;
pub mod protocolo;

pub use credencial::{validar, validar_con, ClaveVerificacion, Credencial, ErrorClave, ErrorCredencial, Rol};
pub use filtro::{Filtro, LimitesOptions};
pub use guardian::Guardian;
pub use limitador::LimitadorKeyframe;
pub use mando::EstadoMando;
pub use protocolo::{leer_mensaje, MsgCliente, MsgServidor};

#[cfg(test)]
mod tests {
    #[test]
    fn el_workspace_compila() {
        assert_eq!(2 + 2, 4);
    }
}
