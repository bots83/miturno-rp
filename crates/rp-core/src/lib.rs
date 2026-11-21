//! Lógica pura de miturno-rp: no depende de libchiaki ni de la red, así se prueba en cualquier máquina.

pub mod credencial;

pub use credencial::{validar, Credencial, ErrorCredencial, Rol};

#[cfg(test)]
mod tests {
    #[test]
    fn el_workspace_compila() {
        assert_eq!(2 + 2, 4);
    }
}
